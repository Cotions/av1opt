// av1opt desktop: runs `av1opt ui` in the background and shows the same page in a window.
// The bundled Chromium plays AV1 (and H.264), so the comparison view needs no browser setup.
//
// Which av1opt binary is used, first match wins:
//   1. $AV1OPT_BIN
//   2. <app resources>/av1opt            (packaged app, see `npm run dist`)
//   3. ../target/release/av1opt          (running from the repository)
//   4. av1opt on $PATH
// Extra arguments for `av1opt ui` go in $AV1OPT_UI_ARGS, e.g. AV1OPT_UI_ARGS="--memory-limit 12G".
// The server's output is kept in $XDG_DATA_HOME/av1opt/desktop.log (Settings → Open log).

"use strict";
const { spawn } = require("node:child_process");

// Started with ELECTRON_RUN_AS_NODE set (inherited from an editor or another Electron app),
// this would run as plain Node with no window. Start again without it.
if (typeof require("electron") === "string") {
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  const again = spawn(process.execPath, process.argv.slice(1), { env, stdio: "inherit" });
  again.on("exit", (code) => process.exit(code ?? 1));
  return;
}

const { app, BrowserWindow, Menu, Notification, dialog, ipcMain, shell, session } = require("electron");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

// The HTTP API this wrapper was written for; the server reports its own in /api/state.
const API_VERSION = 1;

let server = null; // the av1opt ui child process
let origin = null; // http://127.0.0.1:<port>
let token = null;
let quitting = false; // the server is being stopped
let quitConfirmed = false; // the user agreed to stop a running encode (or nothing was running)
const log = []; // last lines of the server's output, shown if it fails

// ---------- log file ----------

const LOG_MAX = 5 * 1024 * 1024; // then desktop.log becomes desktop.log.1 (one old file kept)

function logPath() {
  const base = process.env.XDG_DATA_HOME || path.join(app.getPath("home"), ".local", "share");
  return path.join(base, "av1opt", "desktop.log");
}

let logFd = null;
let logBytes = 0;

function openLog() {
  const p = logPath();
  try {
    fs.mkdirSync(path.dirname(p), { recursive: true });
    try {
      if (fs.statSync(p).size > LOG_MAX) fs.renameSync(p, p + ".1");
    } catch {}
    logFd = fs.openSync(p, "a", 0o600);
    logBytes = fs.fstatSync(logFd).size;
  } catch (e) {
    console.error(`cannot write ${p}: ${e.message}`);
    logFd = null;
  }
}

function writeLog(line) {
  if (logFd === null) return;
  const text = `${new Date().toISOString()} ${line}\n`;
  try {
    fs.writeSync(logFd, text);
    logBytes += Buffer.byteLength(text);
    if (logBytes > LOG_MAX) {
      fs.closeSync(logFd);
      logFd = null;
      openLog();
    }
  } catch {}
}

function findBinary() {
  const candidates = [
    process.env.AV1OPT_BIN,
    app.isPackaged ? path.join(process.resourcesPath, "av1opt") : null,
    path.join(__dirname, "..", "target", "release", "av1opt"),
  ].filter(Boolean);
  for (const c of candidates) {
    try {
      fs.accessSync(c, fs.constants.X_OK);
      return c;
    } catch {}
  }
  return "av1opt";
}

function userArgs() {
  return (process.env.AV1OPT_UI_ARGS || "").split(/\s+/).filter(Boolean);
}

// Starts the server and resolves with its origin once it prints the listening line.
function startServer() {
  token = crypto.randomBytes(24).toString("hex");
  const bin = findBinary();
  writeLog(`--- av1opt desktop ${app.getVersion()} starting ${bin} ${userArgs().join(" ")}`.trimEnd());
  return new Promise((resolve, reject) => {
    const child = spawn(bin, ["ui", "--port", "0", "--no-open", ...userArgs()], {
      // stdin stays open as a lifeline: if this app dies, the server sees EOF and stops its encodes.
      stdio: ["pipe", "inherit", "pipe"],
      env: { ...process.env, AV1OPT_UI_TOKEN: token, AV1OPT_UI_EXIT_WITH_STDIN: "1" },
    });
    server = child;
    const timer = setTimeout(() => reject(new Error("av1opt ui did not start within 20 s")), 20000);
    let buf = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      process.stderr.write(chunk);
      buf += chunk;
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        log.push(line);
        if (log.length > 40) log.shift();
        writeLog(line);
        const m = /^av1opt ui on (http:\/\/127\.0\.0\.1:\d+)\//.exec(line);
        if (m && !origin) {
          clearTimeout(timer);
          origin = m[1];
          resolve();
        }
      }
    });
    child.on("error", (e) => {
      clearTimeout(timer);
      // A failed spawn may never emit "exit".
      if (server === child) server = null;
      reject(new Error(`cannot run ${bin}: ${e.message}`));
    });
    child.on("exit", (code, signal) => {
      clearTimeout(timer);
      server = null;
      writeLog(`--- av1opt ui exited (${signal || "exit code " + code})`);
      if (quitting) return;
      const why = `av1opt ui stopped (${signal || "exit code " + code}).\n\n${log.slice(-15).join("\n")}`;
      if (!origin) {
        reject(new Error(why));
      } else {
        console.error(why);
        dialog.showErrorBox("av1opt", why);
        app.quit();
      }
    });
  });
}

function isOwn(url) {
  try {
    return new URL(url).origin === origin;
  } catch {
    return false;
  }
}

function openExternal(url) {
  if (/^https?:\/\//.test(url) && !isOwn(url)) shell.openExternal(url);
}

function createWindow() {
  // A private, in-memory session: nothing from the page is kept on disk between runs.
  const ses = session.fromPartition("av1opt");
  ses.setPermissionRequestHandler((_wc, _perm, cb) => cb(false));
  ses.setPermissionCheckHandler(() => false);

  const win = new BrowserWindow({
    width: 1400,
    height: 900,
    minWidth: 800,
    minHeight: 560,
    title: "av1opt",
    icon: path.join(__dirname, "icon.png"),
    backgroundColor: "#111315",
    autoHideMenuBar: true,
    show: false,
    webPreferences: {
      session: ses,
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      webviewTag: false,
      spellcheck: false,
    },
  });
  win.once("ready-to-show", () => win.show());

  // The page may only ever show the local server; anything else opens in the normal browser.
  win.webContents.setWindowOpenHandler(({ url }) => {
    openExternal(url);
    return { action: "deny" };
  });
  win.webContents.on("will-navigate", (e, url) => {
    if (!isOwn(url)) {
      e.preventDefault();
      openExternal(url);
    }
  });
  win.webContents.on("will-redirect", (e, url) => {
    if (!isOwn(url)) e.preventDefault();
  });
  win.webContents.on("render-process-gone", (_e, d) => {
    if (!quitting && d.reason !== "clean-exit") win.reload();
  });

  // Closing while an encode runs stops it (it resumes next time); ask first.
  win.on("close", (e) => {
    if (quitConfirmed || quitting) return;
    e.preventDefault();
    confirmQuit(win).then((ok) => ok && !win.isDestroyed() && win.close());
  });

  // Requests from the page, accepted only from our own page in this window.
  const fromPage = (e) => e.sender === win.webContents && isOwn(e.senderFrame ? e.senderFrame.url : "");
  ipcMain.handle("av1opt:pick-folder", async (e, { title, start }) => {
    if (!fromPage(e)) return null;
    const r = await dialog.showOpenDialog(win, {
      title: title || "Choose a folder",
      defaultPath: start && path.isAbsolute(start) ? start : app.getPath("home"),
      properties: ["openDirectory", "createDirectory"],
    });
    return r.canceled || !r.filePaths.length ? null : r.filePaths[0];
  });
  ipcMain.handle("av1opt:open-log", async (e, { folder }) => {
    if (!fromPage(e)) return false;
    const p = logPath();
    if (folder) {
      shell.showItemInFolder(p);
      return true;
    }
    return (await shell.openPath(p)) === "";
  });
  ipcMain.on("av1opt:notify", (e, { title, body }) => {
    if (!fromPage(e) || win.isFocused() || !Notification.isSupported()) return;
    const n = new Notification({ title: String(title).slice(0, 120), body: String(body).slice(0, 300), icon: path.join(__dirname, "icon.png") });
    n.on("click", () => {
      if (win.isDestroyed()) return;
      if (win.isMinimized()) win.restore();
      win.show();
      win.focus();
    });
    n.show();
  });

  win.loadURL(`${origin}/?token=${token}`);
  return win;
}

// Asks before stopping running or queued encodes. One question at a time, answered once:
// closing the window and the quit that follows don't ask twice.
let asking = null;
function confirmQuit(win) {
  if (quitConfirmed) return Promise.resolve(true);
  if (asking) return asking;
  asking = (async () => {
    const { running, queued } = await busy();
    if (running + queued > 0) {
      const opts = {
        type: "question",
        buttons: ["Quit", "Keep open"],
        defaultId: 1,
        cancelId: 1,
        message: running
          ? queued ? `An encode is running and ${queued} more ${queued === 1 ? "job is" : "jobs are"} waiting.` : "An encode is running."
          : `${queued} encode ${queued === 1 ? "job is" : "jobs are"} waiting.`,
        detail: running
          ? "Quitting stops it now. Finished chunks are kept and the job resumes the next time you open av1opt."
          : "Waiting jobs are kept and start the next time you open av1opt.",
      };
      const r = win && !win.isDestroyed() ? await dialog.showMessageBox(win, opts) : await dialog.showMessageBox(opts);
      if (r.response !== 0) return false;
    }
    quitConfirmed = true;
    return true;
  })().finally(() => (asking = null));
  return asking;
}

// Warns once if the server speaks another API version than this wrapper (an av1opt from
// $AV1OPT_BIN or $PATH that is older or newer than the one it was built with).
async function checkApi(win) {
  try {
    const r = await fetch(`${origin}/api/state`, { headers: { Cookie: cookie() }, signal: AbortSignal.timeout(5000) });
    const st = await r.json();
    if (st.api === API_VERSION) return;
    const msg = `The av1opt program (version ${st.version || "unknown"}, API ${st.api ?? "unknown"}) does not match this desktop app (API ${API_VERSION}).`;
    writeLog(`warning: ${msg}`);
    await dialog.showMessageBox(win, {
      type: "warning",
      message: "av1opt versions differ",
      detail: `${msg}\n\nSome views may not work. Use the av1opt that came with this app, or rebuild both from the same version.`,
    });
  } catch (e) {
    writeLog(`warning: could not check the server version: ${e.message}`);
  }
}

function cookie() {
  return `av1opt_${new URL(origin).port}=${token}`;
}

// Jobs running and queued, asked from the server the way the page would.
async function busy() {
  try {
    const r = await fetch(`${origin}/api/jobs/heads`, { headers: { Cookie: cookie() }, signal: AbortSignal.timeout(2000) });
    if (!r.ok) return { running: 0, queued: 0 };
    const { jobs } = await r.json();
    const count = (s) => jobs.filter((j) => j.status === s).length;
    return { running: count("running"), queued: count("queued") };
  } catch {
    return { running: 0, queued: 0 };
  }
}

function stopServer() {
  const child = server;
  if (!child || child.exitCode !== null || child.signalCode !== null || child.pid === undefined) return Promise.resolve();
  return new Promise((resolve) => {
    const kill = setTimeout(() => child.kill("SIGKILL"), 8000);
    child.once("exit", () => {
      clearTimeout(kill);
      resolve();
    });
    // The server stops its ffmpeg processes and keeps unfinished jobs for the next start.
    child.kill("SIGTERM");
  });
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  let mainWindow = null;
  app.on("second-instance", () => {
    if (mainWindow) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.focus();
    }
  });

  app.whenReady().then(async () => {
    try {
      openLog();
      await startServer();
      mainWindow = createWindow();
      mainWindow.on("closed", () => (mainWindow = null));
      mainWindow.webContents.once("did-finish-load", () => checkApi(mainWindow));
    } catch (e) {
      console.error(`av1opt could not start: ${e.message || e}`);
      dialog.showErrorBox("av1opt could not start", String(e.message || e));
      quitting = true;
      await stopServer();
      app.exit(1);
    }
  });

  app.on("window-all-closed", () => app.quit());

  // Every way of quitting ends here, including logout and SIGTERM, which Electron turns into
  // a quit itself; those must not wait on a question. Asking happens where the user acts:
  // closing the window (above) and the Quit menu item / Ctrl+Q (below).
  app.on("before-quit", (e) => {
    if (quitting || !server) return;
    e.preventDefault();
    quitting = true;
    stopServer().then(() => app.quit());
  });

  const userQuit = () => confirmQuit(mainWindow).then((ok) => ok && app.quit());
  Menu.setApplicationMenu(
    Menu.buildFromTemplate([
      { label: "File", submenu: [{ label: "Quit", accelerator: "CmdOrCtrl+Q", click: userQuit }] },
      { role: "editMenu" },
      { label: "View", submenu: [{ role: "reload" }, { role: "toggleDevTools" }, { type: "separator" }, { role: "resetZoom" }, { role: "zoomIn" }, { role: "zoomOut" }, { type: "separator" }, { role: "togglefullscreen" }] },
    ]),
  );
}

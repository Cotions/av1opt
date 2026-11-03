// av1opt desktop: runs `av1opt ui` in the background and shows the same page in a window.
// The bundled Chromium plays AV1 (and H.264), so the comparison view needs no browser setup.
//
// Which av1opt binary is used, first match wins:
//   1. $AV1OPT_BIN
//   2. <app resources>/av1opt            (packaged app, see `npm run dist`)
//   3. ../target/release/av1opt          (running from the repository)
//   4. av1opt on $PATH
// Extra arguments for `av1opt ui` go in $AV1OPT_UI_ARGS, e.g. AV1OPT_UI_ARGS="--memory-limit 12G".

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

const { app, BrowserWindow, dialog, shell, session } = require("electron");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

let server = null; // the av1opt ui child process
let origin = null; // http://127.0.0.1:<port>
let quitting = false;
const log = []; // last lines of the server's output, shown if it fails

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
  const token = crypto.randomBytes(24).toString("hex");
  const bin = findBinary();
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
        const m = /^av1opt ui on (http:\/\/127\.0\.0\.1:\d+)\//.exec(line);
        if (m && !origin) {
          clearTimeout(timer);
          origin = m[1];
          resolve({ origin, token });
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

function createWindow({ origin, token }) {
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

  win.loadURL(`${origin}/?token=${token}`);
  return win;
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
      mainWindow = createWindow(await startServer());
      mainWindow.on("closed", () => (mainWindow = null));
    } catch (e) {
      console.error(`av1opt could not start: ${e.message || e}`);
      dialog.showErrorBox("av1opt could not start", String(e.message || e));
      quitting = true;
      await stopServer();
      app.exit(1);
    }
  });

  app.on("window-all-closed", () => app.quit());

  app.on("before-quit", (e) => {
    if (quitting || !server) return;
    e.preventDefault();
    quitting = true;
    stopServer().then(() => app.quit());
  });
}

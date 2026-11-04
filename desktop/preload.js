// The only bridge between the page and the desktop: a few narrow calls, no Node access.
"use strict";
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("av1optDesktop", {
  /** System folder dialog; resolves to a path or null. */
  pickFolder: (title, start) => ipcRenderer.invoke("av1opt:pick-folder", { title: String(title || ""), start: String(start || "") }),
  /** Desktop notification, shown only while the window is in the background. */
  /** Opens the desktop log file (or, with folder true, shows it in the file manager). */
  openLog: (folder) => ipcRenderer.invoke("av1opt:open-log", { folder: !!folder }),
  notify: (title, body) => ipcRenderer.send("av1opt:notify", { title: String(title || ""), body: String(body || "") }),
});

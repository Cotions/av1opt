// The only bridge between the page and the desktop: two narrow calls, no Node access.
"use strict";
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("av1optDesktop", {
  /** System folder dialog; resolves to a path or null. */
  pickFolder: (title, start) => ipcRenderer.invoke("av1opt:pick-folder", { title: String(title || ""), start: String(start || "") }),
  /** Desktop notification, shown only while the window is in the background. */
  notify: (title, body) => ipcRenderer.send("av1opt:notify", { title: String(title || ""), body: String(body || "") }),
});

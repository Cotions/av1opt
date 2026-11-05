// electron-builder afterPack hook. Electron's zip unpacks with mode 777, and the deb installs
// those modes as root under /opt/av1opt, so any local user could replace the app. Drop group
// and other write on everything packed.
const fs = require("fs");
const path = require("path");

function dropWrite(p) {
  const st = fs.lstatSync(p);
  if (st.isSymbolicLink()) return;
  fs.chmodSync(p, st.mode & 0o7755);
  if (st.isDirectory()) for (const name of fs.readdirSync(p)) dropWrite(path.join(p, name));
}

exports.default = async (context) => dropWrite(context.appOutDir);

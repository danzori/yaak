const path = require("node:path");
const { chmodSync, copyFileSync, existsSync, mkdirSync, statSync } = require("node:fs");
const { execFileSync } = require("node:child_process");

const VERSION = "1.72.0";
const targetArch = process.env.YAAK_TARGET_ARCH ?? process.arch;
const targetKey = `${process.platform}_${targetArch}`;

const PACKAGE_MAP = {
  darwin_arm64: "@bufbuild/buf-darwin-arm64",
  darwin_x64: "@bufbuild/buf-darwin-x64",
  linux_arm64: "@bufbuild/buf-linux-aarch64",
  linux_x64: "@bufbuild/buf-linux-x64",
  win32_arm64: "@bufbuild/buf-win32-arm64",
  win32_x64: "@bufbuild/buf-win32-x64",
};

const packageName = PACKAGE_MAP[targetKey];
if (packageName == null) {
  throw new Error(`Unsupported Buf target ${targetKey}`);
}

const executableName = process.platform === "win32" ? "buf.exe" : "buf";
const destinationName = process.platform === "win32" ? "yaakbuf.exe" : "yaakbuf";
const destinationDirectory = path.join(
  __dirname,
  "..",
  "crates-tauri",
  "yaak-app-client",
  "vendored",
  "buf",
);
const destination = path.join(destinationDirectory, destinationName);

if (existsSync(destination) && readVersion(destination) === VERSION) {
  console.log(`Buf ${VERSION} already vendored for ${targetKey}`);
  return;
}

let packageDirectory;
try {
  packageDirectory = path.dirname(require.resolve(`${packageName}/package.json`));
} catch (error) {
  throw new Error(
    `${packageName} is not installed. Run npm install on the target platform before vendoring Buf.`,
    { cause: error },
  );
}

const source = [
  path.join(packageDirectory, executableName),
  path.join(packageDirectory, "bin", executableName),
].find((candidate) => existsSync(candidate));

if (source == null) {
  throw new Error(`Could not find ${executableName} in ${packageDirectory}`);
}

mkdirSync(destinationDirectory, { recursive: true });
copyFileSync(source, destination);
if (process.platform !== "win32") {
  chmodSync(destination, statSync(destination).mode | 0o700);
}

const actualVersion = readVersion(destination);
if (actualVersion !== VERSION) {
  throw new Error(`Unexpected Buf version ${actualVersion}; expected ${VERSION}`);
}

console.log(`Vendored Buf ${VERSION} for ${targetKey} to ${destination}`);

function readVersion(binary) {
  try {
    return execFileSync(binary, ["--version"], { encoding: "utf8" }).trim();
  } catch {
    return null;
  }
}

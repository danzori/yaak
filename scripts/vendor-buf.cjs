const path = require("node:path");
const os = require("node:os");
const {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} = require("node:fs");
const { execSync } = require("node:child_process");
const { extractArchive } = require("./extract-archive.cjs");

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
const stampPath = path.join(destinationDirectory, ".version");
const stamp = `${packageName}@${VERSION}`;

(async () => {
  if (existsSync(destination) && readStamp() === stamp) {
    console.log(`Buf ${VERSION} already vendored for ${targetKey}`);
    return;
  }

  const tmpDir = mkdtempSync(path.join(os.tmpdir(), "yaak-buf-"));
  try {
    const packageDirectory = findInstalledPackage() ?? (await downloadPackage(tmpDir));

    const packageVersion = JSON.parse(
      readFileSync(path.join(packageDirectory, "package.json"), "utf8"),
    ).version;
    if (packageVersion !== VERSION) {
      throw new Error(`Unexpected ${packageName} version ${packageVersion}; expected ${VERSION}`);
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
      chmodSync(destination, statSync(destination).mode | 0o755);
    }
    writeFileSync(stampPath, stamp);

    console.log(`Vendored Buf ${VERSION} for ${targetKey} to ${destination}`);
  } finally {
    rmSync(tmpDir, { recursive: true, force: true });
  }
})().catch((err) => {
  console.error(err);
  process.exit(1);
});

function findInstalledPackage() {
  try {
    const directory = path.dirname(require.resolve(`${packageName}/package.json`));
    const { version } = JSON.parse(readFileSync(path.join(directory, "package.json"), "utf8"));
    return version === VERSION ? directory : null;
  } catch {
    return null;
  }
}

async function downloadPackage(tmpDir) {
  console.log(`Downloading ${packageName}@${VERSION} from npm`);
  execSync(`npm pack ${packageName}@${VERSION} --pack-destination "${tmpDir}" --silent`, {
    stdio: ["ignore", "ignore", "inherit"],
  });
  const tarball = readdirSync(tmpDir).find((name) => name.endsWith(".tgz"));
  if (tarball == null) {
    throw new Error(`npm pack did not produce a tarball for ${packageName}`);
  }
  const extractDir = path.join(tmpDir, "extract");
  await extractArchive(path.join(tmpDir, tarball), extractDir);
  return path.join(extractDir, "package");
}

function readStamp() {
  try {
    return readFileSync(stampPath, "utf8").trim();
  } catch {
    return null;
  }
}

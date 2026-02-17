import { spawn } from "node:child_process";
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { resolve } from "node:path";

import { chromium } from "playwright";
import pixelmatch from "pixelmatch";
import { PNG } from "pngjs";

const repoRoot = resolve(import.meta.dirname, "../..");
const baselineDir = resolve(repoRoot, "visual-baseline");
const artifactsDir = resolve(repoRoot, "visual-artifacts");
mkdirSync(baselineDir, { recursive: true });
mkdirSync(artifactsDir, { recursive: true });

const mode = process.argv[2] || "test"; // test|update
if (mode !== "test" && mode !== "update") {
  console.error("usage: node scripts/visual/run.mjs <test|update>");
  process.exit(2);
}

function waitForServerUrl(proc, timeoutMs = 240_000) {
  return new Promise((resolveUrl, reject) => {
    let done = false;
    let buf = "";
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      reject(new Error(`timeout waiting for server url\n\noutput:\n${buf.slice(-4000)}`));
    }, timeoutMs);

    const onLine = (line) => {
      buf += `${line}\n`;
      const m = /server listening on http:\/\/(\S+)/.exec(line);
      if (!m) return;
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolveUrl(`http://${m[1]}`);
    };

    proc.stdout.setEncoding("utf8");
    proc.stdout.on("data", (chunk) => {
      for (const line of String(chunk).split("\n")) onLine(line);
    });
    proc.stderr.setEncoding("utf8");
    proc.stderr.on("data", (chunk) => {
      for (const line of String(chunk).split("\n")) onLine(line);
    });

    proc.on("exit", (code) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      reject(new Error(`server exited early: ${code}`));
    });
  });
}

async function main() {
  const dbPath = resolve(artifactsDir, "visual.db");
  // Prefer running the built binary to avoid repeated compile time.
  const bin = resolve(repoRoot, "target/debug/clawdorio-server");
  let server;
  if (existsSync(bin)) {
    server = spawn(
      bin,
      ["--host", "127.0.0.1", "--port", "0", "--db", dbPath],
      { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
  } else {
    server = spawn(
      "cargo",
      [
        "run",
        "-q",
        "-p",
        "clawdorio-server",
        "--",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--db",
        dbPath,
      ],
      { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"] },
    );
  }

  const baseUrl = await waitForServerUrl(server);
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });

  await page.goto(baseUrl, { waitUntil: "networkidle" });
  await page.waitForTimeout(500);

  const pngBytes = await page.screenshot({ fullPage: true });
  await browser.close();

  // Stop the server.
  server.kill("SIGINT");

  const outPng = resolve(artifactsDir, "dashboard.png");
  writeFileSync(outPng, pngBytes);

  const baselinePng = resolve(baselineDir, "dashboard.png");
  if (mode === "update" || !existsSync(baselinePng)) {
    writeFileSync(baselinePng, pngBytes);
    console.log(`baseline written: ${baselinePng}`);
    return;
  }

  const img1 = PNG.sync.read(readFileSync(baselinePng));
  const img2 = PNG.sync.read(pngBytes);
  if (img1.width !== img2.width || img1.height !== img2.height) {
    console.error("baseline size mismatch");
    process.exit(1);
  }

  const diff = new PNG({ width: img1.width, height: img1.height });
  const mismatched = pixelmatch(img1.data, img2.data, diff.data, img1.width, img1.height, {
    threshold: 0.15,
  });
  const diffPng = resolve(artifactsDir, "diff.png");
  writeFileSync(diffPng, PNG.sync.write(diff));

  if (mismatched > 0) {
    console.error(`visual mismatch: ${mismatched} pixels`);
    console.error(`artifact: ${outPng}`);
    console.error(`diff: ${diffPng}`);
    process.exit(1);
  }

  console.log("visual ok");
}

main().catch((e) => {
  console.error(String(e && e.stack ? e.stack : e));
  process.exit(1);
});

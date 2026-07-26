import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { promises as fs } from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  IncrementalSessionReader,
  MAX_ENCODED_BYTES,
  ResponseProcessor,
  SessionLocator,
  buildCodexArgs,
  buildLaunchEnvironment,
  decodeAndSave,
  decodeImage,
  isAllowedHost,
  normalizeBaseUrl,
  startUiServer,
  testApiConnection,
  validateImage,
} from "./codex-image-bridge.mjs";

const PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
const PNG = Buffer.from(PNG_BASE64, "base64");

function minimalJpeg() {
  return Buffer.from([
    0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01,
    0x01, 0x11, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f,
    0x00, 0xff, 0xd9,
  ]);
}

function minimalWebp() {
  return Buffer.from([
    0x52, 0x49, 0x46, 0x46, 18, 0, 0, 0, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
    0x38, 0x4c, 5, 0, 0, 0, 0x2f, 0, 0, 0, 0, 0,
  ]);
}

function imageRecord({ id, turnId, result = PNG_BASE64, status = "generating", event = false }) {
  return {
    type: event ? "event_msg" : "response_item",
    payload: {
      type: event ? "image_generation_end" : "image_generation_call",
      [event ? "call_id" : "id"]: id,
      turn_id: turnId,
      status,
      result,
    },
  };
}

function line(value) {
  return `${JSON.stringify(value)}\n`;
}

async function temporaryDirectory(label) {
  return fs.mkdtemp(path.join(os.tmpdir(), `codex-image-bridge-${label}-`));
}

async function remove(directory) {
  await fs.rm(directory, { recursive: true, force: true });
}

function fileHash(filePath) {
  return fs.readFile(filePath).then((bytes) => createHash("sha256").update(bytes).digest("hex"));
}

test("strictly validates PNG, JPEG, and WebP", () => {
  assert.equal(validateImage(PNG), "png");
  assert.equal(validateImage(minimalJpeg()), "jpg");
  assert.equal(validateImage(minimalWebp()), "webp");

  const corruptPng = Buffer.from(PNG);
  corruptPng[20] ^= 1;
  assert.throws(() => validateImage(corruptPng), /PNG/);
  assert.throws(() => validateImage(Buffer.from([0xff, 0xd8, 0xff])), /JPEG/);
  assert.throws(() => validateImage(Buffer.from("RIFF\u0004\u0000\u0000\u0000WEBP", "binary")), /WebP/);
  assert.throws(
    () => decodeImage(`data:image/jpeg;base64,${PNG_BASE64}`),
    /does not match/,
  );
  assert.throws(() => decodeImage(`${PNG_BASE64.slice(0, -1)}!`), /Base64/);
  assert.throws(() => decodeImage("A".repeat(MAX_ENCODED_BYTES + 1)), /128 MiB/);
});

test("incremental reader excludes old images and accepts both event shapes", async () => {
  const root = await temporaryDirectory("offset");
  try {
    const sessionPath = path.join(root, "rollout.jsonl");
    const oldLine = line(imageRecord({ id: "old", turnId: "old-turn" }));
    await fs.writeFile(sessionPath, oldLine);
    const offset = Buffer.byteLength(oldLine);
    const reader = IncrementalSessionReader.fromOffset(sessionPath, offset, "new-turn");
    await fs.appendFile(
      sessionPath,
      line(imageRecord({ id: "new-call", turnId: "new-turn", status: "working" })) +
        line(imageRecord({ id: "new-end", turnId: "new-turn", event: true, status: "queued" })),
    );
    const snapshot = await reader.refresh();
    assert.deepEqual(
      snapshot.images.map((image) => image.id),
      ["new-call", "new-end"],
    );
    assert.ok(snapshot.images.every((image) => image.status !== "completed"));
    assert.ok(snapshot.images.every((image) => image.result === PNG_BASE64));
  } finally {
    await remove(root);
  }
});

test("incremental reader waits for a complete JSONL line", async () => {
  const root = await temporaryDirectory("truncated");
  try {
    const sessionPath = path.join(root, "rollout.jsonl");
    const complete = line(imageRecord({ id: "image", turnId: "turn" }));
    await fs.writeFile(sessionPath, complete.slice(0, -1));
    const reader = IncrementalSessionReader.fromStart(sessionPath);
    assert.equal((await reader.refresh()).images.length, 0);
    assert.equal(reader.offset, 0);
    await fs.appendFile(sessionPath, "\n");
    assert.equal((await reader.refresh()).images.length, 1);
    assert.equal(reader.offset, Buffer.byteLength(complete));
  } finally {
    await remove(root);
  }
});

test("saved_path reuse requires a matching SHA256 and storage deduplicates", async () => {
  const root = await temporaryDirectory("saved-path");
  try {
    const existing = path.join(root, "existing.png");
    const output = path.join(root, "output");
    await fs.writeFile(existing, PNG);
    const image = { id: "image", result: PNG_BASE64, savedPath: existing };
    const reused = await decodeAndSave(output, "thread", image);
    assert.equal(reused.path, await fs.realpath(existing));
    await assert.rejects(fs.stat(output), { code: "ENOENT" });

    await fs.writeFile(existing, minimalJpeg());
    const first = await decodeAndSave(output, "thread", image);
    const second = await decodeAndSave(output, "thread", image);
    assert.equal(first.path, second.path);
    assert.equal(first.sha256, createHash("sha256").update(PNG).digest("hex"));
    assert.match(path.basename(first.path), new RegExp(`^${first.sha256}\\.png$`));
  } finally {
    await remove(root);
  }
});

test("SessionLocator uses a read-only state database before scanning", async (context) => {
  let DatabaseSync;
  try {
    ({ DatabaseSync } = await import("node:sqlite"));
  } catch {
    context.skip("node:sqlite is unavailable");
    return;
  }
  const root = await temporaryDirectory("sqlite");
  try {
    const sessions = path.join(root, "sessions");
    await fs.mkdir(sessions);
    const rollout = path.join(sessions, "rollout-without-target-name.jsonl");
    await fs.writeFile(rollout, "");
    const databasePath = path.join(root, "state_5.sqlite");
    const database = new DatabaseSync(databasePath);
    database.exec("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL)");
    database.prepare("INSERT INTO threads (id, rollout_path) VALUES (?, ?)").run("thread-123", rollout);
    database.close();
    const hashBefore = await fileHash(databasePath);
    const locator = new SessionLocator(root);
    assert.equal(await locator.locateFast("thread-123"), await fs.realpath(rollout));
    assert.equal(await fileHash(databasePath), hashBefore);
    await assert.rejects(fs.stat(`${databasePath}-journal`), { code: "ENOENT" });
  } finally {
    await remove(root);
  }
});

test("realtime injection uses the turn offset, emits a pair, and suppresses duplicates", async () => {
  const root = await temporaryDirectory("realtime");
  try {
    const startedAtMs = Date.now() - 1000;
    const sessionPath = path.join(root, "rollout-thread.jsonl");
    const initial = line(imageRecord({ id: "old", turnId: "old-turn" }));
    await fs.writeFile(sessionPath, initial);
    const sessions = new SessionLocator(root);
    sessions.remember("thread", sessionPath);
    const pendingTurns = new Map([
      [
        "thread",
        [
          {
            threadId: "thread",
            startedAtMs,
            sessionPath,
            sessionOffset: Buffer.byteLength(initial),
          },
        ],
      ],
    ]);
    const processor = new ResponseProcessor({
      sessions,
      pendingTurns,
      outputRoot: path.join(root, "images"),
      sleep: async () => {},
    });
    await processor.registerTurnGuard("thread", "turn");
    await fs.appendFile(
      sessionPath,
      line(imageRecord({ id: "image-1", turnId: "turn", event: true, status: "generating" })),
    );
    const sessionHashBefore = await fileHash(sessionPath);
    const completed = Buffer.from(
      line({
        method: "turn/completed",
        params: { threadId: "thread", turn: { id: "turn", items: [{ type: "agentMessage" }] } },
      }),
    );
    const messages = await processor.processLine(completed);
    assert.equal(messages.length, 3);
    const started = JSON.parse(messages[0]);
    const injected = JSON.parse(messages[1]);
    assert.equal(started.method, "item/started");
    assert.equal(started.params.startedAtMs, startedAtMs);
    assert.equal(injected.method, "item/completed");
    assert.equal(injected.params.item.result, "");
    assert.equal(injected.params.item.status, "completed");
    assert.ok((await fs.stat(injected.params.item.savedPath)).isFile());
    assert.equal((await processor.processLine(completed)).length, 1);
    assert.equal(await fileHash(sessionPath), sessionHashBefore);
  } finally {
    await remove(root);
  }
});

test("official image items are materialized and hash-deduplicated", async () => {
  const root = await temporaryDirectory("official");
  try {
    const sessionPath = path.join(root, "rollout.jsonl");
    await fs.writeFile(sessionPath, line(imageRecord({ id: "session-id", turnId: "turn" })));
    const sessions = new SessionLocator(root);
    sessions.remember("thread", sessionPath);
    const processor = new ResponseProcessor({ sessions, outputRoot: path.join(root, "images"), sleep: async () => {} });
    const official = Buffer.from(
      line({
        method: "item/completed",
        params: {
          threadId: "thread",
          turnId: "turn",
          item: { type: "imageGeneration", id: "official-id", status: "working", result: PNG_BASE64 },
        },
      }),
    );
    const normalized = JSON.parse((await processor.processLine(official))[0]);
    assert.equal(normalized.params.item.result, "");
    assert.ok((await fs.stat(normalized.params.item.savedPath)).isFile());

    const completed = Buffer.from(
      line({ method: "turn/completed", params: { threadId: "thread", turn: { id: "turn", items: [] } } }),
    );
    assert.equal((await processor.processLine(completed)).length, 1);
  } finally {
    await remove(root);
  }
});

test("history responses receive imageGeneration items before the agent message", async () => {
  const root = await temporaryDirectory("history");
  try {
    const sessionPath = path.join(root, "rollout.jsonl");
    await fs.writeFile(sessionPath, line(imageRecord({ id: "history-image", turnId: "turn" })));
    const sessions = new SessionLocator(root);
    const requests = new Map([[JSON.stringify(7), { method: "thread/read" }]]);
    const processor = new ResponseProcessor({ requests, sessions, outputRoot: path.join(root, "images") });
    const response = Buffer.from(
      line({
        id: 7,
        result: {
          thread: {
            id: "thread",
            path: sessionPath,
            turns: [{ id: "turn", items: [{ type: "userMessage" }, { type: "agentMessage" }] }],
          },
        },
      }),
    );
    const restored = JSON.parse((await processor.processLine(response))[0]);
    const items = restored.result.thread.turns[0].items;
    assert.deepEqual(items.map((item) => item.type), ["userMessage", "imageGeneration", "agentMessage"]);
    assert.equal(items[1].result, "");
    assert.ok((await fs.stat(items[1].savedPath)).isFile());
  } finally {
    await remove(root);
  }
});

test("provider arguments contain no API Key", () => {
  const secret = "test-secret-never-in-argv";
  const args = buildCodexArgs(["app-server"], "https://example.com", true);
  assert.equal(JSON.stringify(args).includes(secret), false);
  assert.ok(args.includes("model_providers.comidea.requires_openai_auth=true"));
  assert.ok(args.includes("features.image_generation=true"));
  assert.ok(args.some((argument) => argument.includes('base_url="https://example.com/v1"')));

  const environment = buildLaunchEnvironment(
    { baseUrl: "https://example.com/v1", apiKey: secret, imageEnabled: true },
    { realCli: "/Applications/Codex.app/Contents/Resources/codex" },
    "/tmp/codex-image-bridge.mjs",
    {},
  );
  assert.equal(environment.COMIDEA_API_KEY, secret);
  assert.equal(environment.NODE_NO_WARNINGS, "1");
  assert.equal(JSON.stringify(args).includes(environment.COMIDEA_API_KEY), false);
  assert.equal(normalizeBaseUrl("http://127.0.0.1:8000"), "http://127.0.0.1:8000/v1");
  assert.throws(() => normalizeBaseUrl("http://example.com/v1"), /HTTPS/);
  assert.throws(() => normalizeBaseUrl("https://example.com/v1/v1"), /重复/);
});

test("connection test sends the Key only as Authorization and detects gpt-image-2", async () => {
  const secret = "test-connection-secret";
  const server = http.createServer((request, response) => {
    assert.equal(request.url, "/v1/models");
    assert.equal(request.headers.authorization, `Bearer ${secret}`);
    response.writeHead(200, { "Content-Type": "application/json" });
    response.end(JSON.stringify({ data: [{ id: "text-model" }, { id: "gpt-image-2" }] }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    const result = await testApiConnection({
      baseUrl: `http://127.0.0.1:${server.address().port}`,
      apiKey: secret,
    });
    assert.deepEqual(result, {
      reachable: true,
      authenticated: true,
      modelFound: true,
      imageModel: "gpt-image-2",
    });
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});

test("app-server proxy exits when the real CLI exits while parent stdin stays open", async () => {
  const bridgePath = fileURLToPath(new URL("./codex-image-bridge.mjs", import.meta.url));
  const child = spawn(
    process.execPath,
    [bridgePath, "--eval", "process.exit(0)", "app-server"],
    {
      env: {
        ...process.env,
        CODEX_IMAGE_BRIDGE_REAL_CLI: process.execPath,
        CODEX_IMAGE_BRIDGE_ENABLED: "0",
      },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    },
  );
  let timeout;
  let result;
  try {
    result = await Promise.race([
      new Promise((resolve, reject) => {
        child.once("error", reject);
        child.once("exit", (code, signal) => resolve({ code, signal }));
      }),
      new Promise((_, reject) => {
        timeout = setTimeout(() => reject(new Error("bridge did not exit after its child")), 5000);
      }),
    ]);
  } finally {
    clearTimeout(timeout);
    child.stdin.destroy();
    if (child.exitCode === null && child.signalCode === null) child.kill();
  }
  assert.deepEqual(result, { code: 0, signal: null });
});

function rawRequest({ port, method = "GET", route = "/", headers = {}, body }) {
  return new Promise((resolve, reject) => {
    const request = http.request(
      { host: "127.0.0.1", port, method, path: route, headers },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks).toString("utf8") }));
      },
    );
    request.on("error", reject);
    if (body) request.write(body);
    request.end();
  });
}

test("local UI rejects invalid Host, Origin, CSRF, and content type", async () => {
  const secret = "test-ui-secret";
  const ui = await startUiServer({
    openBrowser: false,
    shutdownAfterLaunch: false,
    probe: async () => ({ platform: "darwin", architecture: "arm64", node: "24.0.0", nodeSupported: true, codexFound: true }),
    launch: async () => ({ launched: true }),
    testConnection: async () => ({ reachable: true, authenticated: true, modelFound: true }),
  });
  try {
    assert.equal(isAllowedHost(`127.0.0.1:${ui.port}`, ui.port), true);
    assert.equal((await rawRequest({ port: ui.port, headers: { Host: "evil.example" } })).status, 421);

    const bootstrap = await fetch(ui.bootstrapUrl, { redirect: "manual" });
    assert.equal(bootstrap.status, 303);
    const cookie = bootstrap.headers.get("set-cookie").split(";", 1)[0];
    const page = await fetch(`${ui.url}/`, { headers: { Cookie: cookie } });
    const html = await page.text();
    const csrf = html.match(/meta name="csrf-token" content="([^"]+)"/)[1];
    const common = { Cookie: cookie, "Content-Type": "application/json", "X-CSRF-Token": csrf };

    const badOrigin = await fetch(`${ui.url}/api/save`, {
      method: "POST",
      headers: { ...common, Origin: "https://evil.example" },
      body: "{}",
    });
    assert.equal(badOrigin.status, 403);
    const badCsrf = await fetch(`${ui.url}/api/save`, {
      method: "POST",
      headers: { ...common, Origin: ui.url, "X-CSRF-Token": "wrong" },
      body: "{}",
    });
    assert.equal(badCsrf.status, 403);
    const badType = await fetch(`${ui.url}/api/save`, {
      method: "POST",
      headers: { Cookie: cookie, Origin: ui.url, "X-CSRF-Token": csrf, "Content-Type": "text/plain" },
      body: "{}",
    });
    assert.equal(badType.status, 415);

    const saved = await fetch(`${ui.url}/api/save`, {
      method: "POST",
      headers: { ...common, Origin: ui.url },
      body: JSON.stringify({ baseUrl: "https://example.com/v1", apiKey: secret, imageEnabled: true }),
    });
    assert.equal(saved.status, 200);
    const status = await fetch(`${ui.url}/api/status`, { headers: { Cookie: cookie } });
    assert.equal((await status.text()).includes(secret), false);
  } finally {
    await ui.close();
  }
});

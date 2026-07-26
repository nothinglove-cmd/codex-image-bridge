#!/usr/bin/env node

import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { once } from "node:events";
import { constants as fsConstants, promises as fs } from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const IMAGE_MODEL = "gpt-image-2";
export const MAX_ENCODED_BYTES = 128 * 1024 * 1024;
export const MAX_JSONL_LINE_BYTES = MAX_ENCODED_BYTES + 1024 * 1024;

const MAX_IMAGE_BYTES = MAX_ENCODED_BYTES;
const MAX_HTTP_BODY_BYTES = 64 * 1024;
const MAX_HTTP_RESPONSE_BYTES = 2 * 1024 * 1024;
const SEEN_CACHE_CAPACITY = 4096;
const RETRY_DELAYS_MS = [75, 200, 500];
const TURN_GUARD_TTL_MS = 60 * 60 * 1000;
const BRIDGE_ENVIRONMENT_VARIABLES = [
  "CODEX_CLI_PATH",
  "CODEX_IMAGE_BRIDGE_REAL_CLI",
  "CODEX_IMAGE_BRIDGE_BASE_URL",
  "CODEX_IMAGE_BRIDGE_IMAGE_ENABLED",
  "CODEX_IMAGE_BRIDGE_ENABLED",
];

const MODULE_PATH = fileURLToPath(import.meta.url);
const PNG_CRC_TABLE = createPngCrcTable();

function fail(message, code = "BRIDGE_ERROR") {
  const error = new Error(message);
  error.code = code;
  return error;
}

function safeWarning(context, error) {
  const code = typeof error?.code === "string" ? error.code : "BRIDGE_ERROR";
  process.stderr.write(`codex-image-bridge: ${context} (${code})\n`);
}

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function getAt(value, keys) {
  let current = value;
  for (const key of keys) {
    if (current === null || typeof current !== "object") return undefined;
    current = current[key];
  }
  return current;
}

function asString(value) {
  return typeof value === "string" ? value : undefined;
}

function requestKey(value) {
  if (value?.id === undefined || value.id === null) return undefined;
  try {
    return JSON.stringify(value.id);
  } catch {
    return undefined;
  }
}

function threadIdFromTurnStart(value) {
  return (
    asString(getAt(value, ["params", "threadId"])) ??
    asString(getAt(value, ["params", "thread", "id"])) ??
    asString(getAt(value, ["params", "thread_id"]))
  );
}

function responseTurnId(value) {
  return (
    asString(getAt(value, ["result", "turn", "id"])) ??
    asString(getAt(value, ["result", "id"]))
  );
}

function isHistoryMethod(method) {
  return ["thread/read", "thread/resume", "thread/fork", "thread/rollback"].includes(method);
}

function turnKey(threadId, turnId) {
  return `${threadId}\u001f${turnId}`;
}

function imageIdKey(threadId, turnId, imageId) {
  return `id\u001f${threadId}\u001f${turnId}\u001f${imageId}`;
}

function imageHashKey(threadId, turnId, hash) {
  return `sha256\u001f${threadId}\u001f${turnId}\u001f${hash}`;
}

export function codexHome(environment = process.env) {
  return environment.CODEX_HOME || path.join(os.homedir(), ".codex");
}

export function defaultOutputDirectory(environment = process.env) {
  return path.join(codexHome(environment), "generated-images");
}

export function sanitizePathComponent(value) {
  const sanitized = String(value).replace(/[^A-Za-z0-9._-]/g, "_");
  return sanitized || "unknown";
}

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function createPngCrcTable() {
  const table = new Uint32Array(256);
  for (let index = 0; index < table.length; index += 1) {
    let value = index;
    for (let bit = 0; bit < 8; bit += 1) {
      value = value & 1 ? (value >>> 1) ^ 0xedb88320 : value >>> 1;
    }
    table[index] = value >>> 0;
  }
  return table;
}

function pngCrc32(chunkType, data) {
  let crc = 0xffffffff;
  for (const bytes of [chunkType, data]) {
    for (const byte of bytes) {
      crc = PNG_CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function validatePng(bytes) {
  if (bytes.length < 45) throw fail("Truncated PNG", "INVALID_IMAGE");
  let offset = 8;
  let firstChunk = true;
  let sawIdat = false;
  while (true) {
    const headerEnd = offset + 8;
    if (!Number.isSafeInteger(headerEnd) || headerEnd > bytes.length) {
      throw fail("Truncated PNG chunk header", "INVALID_IMAGE");
    }
    const length = bytes.readUInt32BE(offset);
    const chunkType = bytes.subarray(offset + 4, offset + 8);
    if (![...chunkType].every((byte) => (byte >= 65 && byte <= 90) || (byte >= 97 && byte <= 122))) {
      throw fail("Invalid PNG chunk type", "INVALID_IMAGE");
    }
    const dataEnd = headerEnd + length;
    const chunkEnd = dataEnd + 4;
    if (!Number.isSafeInteger(chunkEnd) || chunkEnd > bytes.length) {
      throw fail("Truncated PNG chunk", "INVALID_IMAGE");
    }
    const expectedCrc = bytes.readUInt32BE(dataEnd);
    if (pngCrc32(chunkType, bytes.subarray(headerEnd, dataEnd)) !== expectedCrc) {
      throw fail("PNG CRC mismatch", "INVALID_IMAGE");
    }
    const type = chunkType.toString("ascii");
    if (type === "IHDR") {
      if (!firstChunk || length !== 13) throw fail("Invalid PNG IHDR", "INVALID_IMAGE");
      const data = bytes.subarray(headerEnd, dataEnd);
      const width = data.readUInt32BE(0);
      const height = data.readUInt32BE(4);
      const bitDepth = data[8];
      const colorType = data[9];
      const validDepth =
        (colorType === 0 && [1, 2, 4, 8, 16].includes(bitDepth)) ||
        ([2, 4, 6].includes(colorType) && [8, 16].includes(bitDepth)) ||
        (colorType === 3 && [1, 2, 4, 8].includes(bitDepth));
      if (!width || !height || !validDepth || data[10] !== 0 || data[11] !== 0 || data[12] > 1) {
        throw fail("Invalid PNG IHDR fields", "INVALID_IMAGE");
      }
    } else if (type === "IDAT") {
      sawIdat = true;
    } else if (type === "IEND") {
      if (length !== 0 || !sawIdat || chunkEnd !== bytes.length) {
        throw fail("Invalid PNG IEND", "INVALID_IMAGE");
      }
      return;
    } else if (firstChunk) {
      throw fail("PNG does not start with IHDR", "INVALID_IMAGE");
    }
    firstChunk = false;
    offset = chunkEnd;
  }
}

function isJpegFrameMarker(marker) {
  return (
    (marker >= 0xc0 && marker <= 0xc3) ||
    (marker >= 0xc5 && marker <= 0xc7) ||
    (marker >= 0xc9 && marker <= 0xcb) ||
    (marker >= 0xcd && marker <= 0xcf)
  );
}

function nextJpegScanMarker(bytes, initialOffset) {
  let offset = initialOffset;
  while (offset < bytes.length) {
    if (bytes[offset] !== 0xff) {
      offset += 1;
      continue;
    }
    offset += 1;
    while (offset < bytes.length && bytes[offset] === 0xff) offset += 1;
    if (offset >= bytes.length) throw fail("Truncated JPEG scan", "INVALID_IMAGE");
    const marker = bytes[offset];
    offset += 1;
    if (marker === 0x00 || (marker >= 0xd0 && marker <= 0xd7)) continue;
    return [marker, offset];
  }
  throw fail("JPEG has no EOI", "INVALID_IMAGE");
}

function validateJpeg(bytes) {
  if (bytes.length < 4 || bytes[0] !== 0xff || bytes[1] !== 0xd8) {
    throw fail("Invalid JPEG SOI", "INVALID_IMAGE");
  }
  let offset = 2;
  let pendingMarker;
  let sawFrame = false;
  while (true) {
    let marker;
    if (pendingMarker !== undefined) {
      marker = pendingMarker;
      pendingMarker = undefined;
    } else {
      if (offset >= bytes.length || bytes[offset] !== 0xff) {
        throw fail("Invalid JPEG marker boundary", "INVALID_IMAGE");
      }
      while (offset < bytes.length && bytes[offset] === 0xff) offset += 1;
      if (offset >= bytes.length) throw fail("Truncated JPEG marker", "INVALID_IMAGE");
      marker = bytes[offset];
      offset += 1;
    }
    if (marker === 0xd9) {
      if (!sawFrame || offset !== bytes.length) throw fail("Invalid JPEG EOI", "INVALID_IMAGE");
      return;
    }
    if (marker === 0xd8 || marker === 0x00 || marker === 0xff) {
      throw fail("Invalid JPEG marker", "INVALID_IMAGE");
    }
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd7)) continue;
    const lengthEnd = offset + 2;
    if (lengthEnd > bytes.length) throw fail("Truncated JPEG segment length", "INVALID_IMAGE");
    const length = bytes.readUInt16BE(offset);
    if (length < 2) throw fail("Invalid JPEG segment length", "INVALID_IMAGE");
    const segmentEnd = offset + length;
    if (!Number.isSafeInteger(segmentEnd) || segmentEnd > bytes.length) {
      throw fail("Truncated JPEG segment", "INVALID_IMAGE");
    }
    const data = bytes.subarray(lengthEnd, segmentEnd);
    if (isJpegFrameMarker(marker)) {
      if (data.length < 6) throw fail("Truncated JPEG frame", "INVALID_IMAGE");
      const height = data.readUInt16BE(1);
      const width = data.readUInt16BE(3);
      const components = data[5];
      if (!width || !height || !components || data.length !== 6 + 3 * components) {
        throw fail("Invalid JPEG frame", "INVALID_IMAGE");
      }
      sawFrame = true;
    }
    offset = segmentEnd;
    if (marker === 0xda) {
      if (data.length < 4) throw fail("Invalid JPEG scan header", "INVALID_IMAGE");
      const components = data[0];
      if (!components || data.length !== 4 + 2 * components) {
        throw fail("Invalid JPEG scan header", "INVALID_IMAGE");
      }
      [pendingMarker, offset] = nextJpegScanMarker(bytes, offset);
    }
  }
}

function readU24Le(bytes, offset) {
  return bytes[offset] | (bytes[offset + 1] << 8) | (bytes[offset + 2] << 16);
}

function validateVp8(data) {
  if (data.length < 10 || data[3] !== 0x9d || data[4] !== 0x01 || data[5] !== 0x2a) {
    throw fail("Invalid WebP VP8 frame", "INVALID_IMAGE");
  }
  const width = data.readUInt16LE(6) & 0x3fff;
  const height = data.readUInt16LE(8) & 0x3fff;
  if (!width || !height) throw fail("Invalid WebP VP8 dimensions", "INVALID_IMAGE");
}

function validateVp8l(data) {
  if (data.length < 5 || data[0] !== 0x2f || data[4] >> 5 !== 0) {
    throw fail("Invalid WebP VP8L frame", "INVALID_IMAGE");
  }
  const width = 1 + data[1] + ((data[2] & 0x3f) << 8);
  const height = 1 + (data[2] >> 6) + (data[3] << 2) + ((data[4] & 0x0f) << 10);
  if (!width || !height) throw fail("Invalid WebP VP8L dimensions", "INVALID_IMAGE");
}

function validateVp8x(data) {
  if (data.length !== 10 || data[1] !== 0 || data[2] !== 0 || data[3] !== 0) {
    throw fail("Invalid WebP VP8X header", "INVALID_IMAGE");
  }
  const width = 1 + readU24Le(data, 4);
  const height = 1 + readU24Le(data, 7);
  if (!width || !height) throw fail("Invalid WebP VP8X dimensions", "INVALID_IMAGE");
}

function validateAnmf(data) {
  if (data.length < 24) throw fail("Truncated WebP animation frame", "INVALID_IMAGE");
  const width = 1 + readU24Le(data, 6);
  const height = 1 + readU24Le(data, 9);
  if (!width || !height) throw fail("Invalid WebP animation dimensions", "INVALID_IMAGE");
  let offset = 16;
  let sawImage = false;
  while (offset < data.length) {
    const headerEnd = offset + 8;
    if (headerEnd > data.length) throw fail("Truncated WebP ANMF chunk", "INVALID_IMAGE");
    const length = data.readUInt32LE(offset + 4);
    const dataEnd = headerEnd + length;
    const chunkEnd = dataEnd + (length & 1);
    if (!Number.isSafeInteger(chunkEnd) || chunkEnd > data.length) {
      throw fail("Truncated WebP ANMF data", "INVALID_IMAGE");
    }
    const type = data.subarray(offset, offset + 4).toString("ascii");
    if (type === "VP8 ") {
      validateVp8(data.subarray(headerEnd, dataEnd));
      sawImage = true;
    } else if (type === "VP8L") {
      validateVp8l(data.subarray(headerEnd, dataEnd));
      sawImage = true;
    }
    offset = chunkEnd;
  }
  if (!sawImage) throw fail("WebP animation frame has no image data", "INVALID_IMAGE");
}

function validateWebp(bytes) {
  if (
    bytes.length < 20 ||
    bytes.subarray(0, 4).toString("ascii") !== "RIFF" ||
    bytes.subarray(8, 12).toString("ascii") !== "WEBP"
  ) {
    throw fail("Invalid WebP header", "INVALID_IMAGE");
  }
  if (bytes.readUInt32LE(4) + 8 !== bytes.length) {
    throw fail("Invalid WebP RIFF length", "INVALID_IMAGE");
  }
  let offset = 12;
  let sawImage = false;
  while (offset < bytes.length) {
    const headerEnd = offset + 8;
    if (headerEnd > bytes.length) throw fail("Truncated WebP chunk header", "INVALID_IMAGE");
    const type = bytes.subarray(offset, offset + 4).toString("ascii");
    const length = bytes.readUInt32LE(offset + 4);
    const dataEnd = headerEnd + length;
    const chunkEnd = dataEnd + (length & 1);
    if (!Number.isSafeInteger(chunkEnd) || chunkEnd > bytes.length) {
      throw fail("Truncated WebP chunk", "INVALID_IMAGE");
    }
    if ((length & 1) === 1 && bytes[dataEnd] !== 0) {
      throw fail("Invalid WebP padding", "INVALID_IMAGE");
    }
    const data = bytes.subarray(headerEnd, dataEnd);
    if (type === "VP8 ") {
      validateVp8(data);
      sawImage = true;
    } else if (type === "VP8L") {
      validateVp8l(data);
      sawImage = true;
    } else if (type === "VP8X") {
      validateVp8x(data);
    } else if (type === "ANMF") {
      validateAnmf(data);
      sawImage = true;
    }
    offset = chunkEnd;
  }
  if (!sawImage) throw fail("WebP has no image data", "INVALID_IMAGE");
}

export function validateImage(bytes) {
  if (!Buffer.isBuffer(bytes)) bytes = Buffer.from(bytes);
  if (bytes.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))) {
    validatePng(bytes);
    return "png";
  }
  if (bytes[0] === 0xff && bytes[1] === 0xd8) {
    validateJpeg(bytes);
    return "jpg";
  }
  if (bytes.subarray(0, 4).toString("ascii") === "RIFF") {
    validateWebp(bytes);
    return "webp";
  }
  throw fail("Unsupported image signature", "INVALID_IMAGE");
}

function base64Value(characterCode) {
  if (characterCode >= 65 && characterCode <= 90) return characterCode - 65;
  if (characterCode >= 97 && characterCode <= 122) return characterCode - 71;
  if (characterCode >= 48 && characterCode <= 57) return characterCode + 4;
  if (characterCode === 43) return 62;
  if (characterCode === 47) return 63;
  return -1;
}

function decodeBase64Strict(encoded) {
  if (!encoded.length || encoded.length % 4 !== 0) {
    throw fail("Invalid image Base64", "INVALID_BASE64");
  }
  let padding = 0;
  if (encoded.endsWith("==")) padding = 2;
  else if (encoded.endsWith("=")) padding = 1;
  const contentLength = encoded.length - padding;
  for (let index = 0; index < contentLength; index += 1) {
    if (base64Value(encoded.charCodeAt(index)) < 0) {
      throw fail("Invalid image Base64", "INVALID_BASE64");
    }
  }
  for (let index = contentLength; index < encoded.length; index += 1) {
    if (encoded.charCodeAt(index) !== 61) throw fail("Invalid image Base64", "INVALID_BASE64");
  }
  if (padding === 2 && (base64Value(encoded.charCodeAt(contentLength - 1)) & 0x0f) !== 0) {
    throw fail("Non-canonical image Base64", "INVALID_BASE64");
  }
  if (padding === 1 && (base64Value(encoded.charCodeAt(contentLength - 1)) & 0x03) !== 0) {
    throw fail("Non-canonical image Base64", "INVALID_BASE64");
  }
  const bytes = Buffer.from(encoded, "base64");
  const expectedLength = (encoded.length / 4) * 3 - padding;
  if (bytes.length !== expectedLength) throw fail("Invalid image Base64", "INVALID_BASE64");
  return bytes;
}

export function decodeImage(value) {
  if (typeof value !== "string") throw fail("Image result is not text", "INVALID_BASE64");
  let encoded = value;
  let declaredExtension;
  if (value.startsWith("data:")) {
    const separator = value.indexOf(",");
    if (separator < 0) throw fail("Invalid image data URI", "INVALID_BASE64");
    const metadata = value.slice(0, separator).toLowerCase();
    declaredExtension = new Map([
      ["data:image/png;base64", "png"],
      ["data:image/jpeg;base64", "jpg"],
      ["data:image/webp;base64", "webp"],
    ]).get(metadata);
    if (!declaredExtension) throw fail("Unsupported image data URI", "INVALID_IMAGE");
    encoded = value.slice(separator + 1);
  }
  if (encoded.length > MAX_ENCODED_BYTES) {
    throw fail("Encoded image exceeds 128 MiB limit", "IMAGE_TOO_LARGE");
  }
  const bytes = decodeBase64Strict(encoded);
  const extension = validateImage(bytes);
  if (declaredExtension && declaredExtension !== extension) {
    throw fail("Image data URI type does not match content", "INVALID_IMAGE");
  }
  return { bytes, extension, sha256: sha256(bytes) };
}

export async function inspectImageFile(filePath) {
  const metadata = await fs.lstat(filePath);
  if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size > MAX_IMAGE_BYTES) {
    throw fail("Saved image is not a supported regular file", "INVALID_IMAGE");
  }
  const bytes = await fs.readFile(filePath);
  validateImage(bytes);
  return { path: await fs.realpath(filePath), sha256: sha256(bytes) };
}

async function syncDirectory(directory) {
  let handle;
  try {
    handle = await fs.open(directory, "r");
    await handle.sync();
  } catch (error) {
    if (process.platform !== "win32" || !["EINVAL", "EPERM", "EISDIR", "EBADF"].includes(error?.code)) {
      throw error;
    }
  } finally {
    await handle?.close().catch(() => {});
  }
}

async function atomicWriteIfChanged(destination, bytes, expectedHash) {
  try {
    const existing = await inspectImageFile(destination);
    if (existing.sha256 === expectedHash) return;
  } catch (error) {
    if (error?.code !== "ENOENT" && error?.code !== "INVALID_IMAGE") throw error;
  }
  const directory = path.dirname(destination);
  const temporary = path.join(
    directory,
    `.${path.basename(destination)}.${process.pid}.${randomBytes(8).toString("hex")}.tmp`,
  );
  let handle;
  try {
    handle = await fs.open(temporary, "wx", 0o600);
    await handle.writeFile(bytes);
    await handle.sync();
    await handle.close();
    handle = undefined;
    await fs.rename(temporary, destination);
    await syncDirectory(directory);
  } catch (error) {
    await handle?.close().catch(() => {});
    await fs.unlink(temporary).catch(() => {});
    throw error;
  }
}

export async function decodeAndSave(outputRoot, threadId, image) {
  let existing;
  if (image.savedPath) {
    try {
      existing = await inspectImageFile(image.savedPath);
    } catch {
      existing = undefined;
    }
  }
  let decoded;
  if (typeof image.result === "string" && image.result.length > 0) {
    decoded = decodeImage(image.result);
  }
  if (existing && (!decoded || decoded.sha256 === existing.sha256)) return existing;
  if (!decoded) throw fail("Image has neither valid saved_path nor result", "IMAGE_NOT_READY");

  await fs.mkdir(outputRoot, { recursive: true, mode: 0o700 });
  const rootMetadata = await fs.lstat(outputRoot);
  if (!rootMetadata.isDirectory() || rootMetadata.isSymbolicLink()) {
    throw fail("Image output root must be a real directory", "UNSAFE_OUTPUT_PATH");
  }
  const canonicalRoot = await fs.realpath(outputRoot);
  const threadDirectory = path.join(canonicalRoot, sanitizePathComponent(threadId));
  const existingThreadDirectory = await fs.lstat(threadDirectory).catch((error) => {
    if (error?.code === "ENOENT") return undefined;
    throw error;
  });
  if (existingThreadDirectory?.isSymbolicLink() || (existingThreadDirectory && !existingThreadDirectory.isDirectory())) {
    throw fail("Image thread directory is unsafe", "UNSAFE_OUTPUT_PATH");
  }
  await fs.mkdir(threadDirectory, { recursive: false, mode: 0o700 }).catch((error) => {
    if (error?.code !== "EEXIST") throw error;
  });
  const canonicalThreadDirectory = await fs.realpath(threadDirectory);
  if (!pathIsWithin(canonicalThreadDirectory, canonicalRoot)) {
    throw fail("Image thread directory escaped its root", "UNSAFE_OUTPUT_PATH");
  }
  await fs.chmod(canonicalThreadDirectory, 0o700).catch(() => {});
  const destination = path.join(canonicalThreadDirectory, `${decoded.sha256}.${decoded.extension}`);
  await atomicWriteIfChanged(destination, decoded.bytes, decoded.sha256);
  return { path: await fs.realpath(destination), sha256: decoded.sha256 };
}

export async function* splitLines(stream, maximumBytes = MAX_JSONL_LINE_BYTES) {
  let pieces = [];
  let size = 0;
  for await (const rawChunk of stream) {
    const chunk = Buffer.isBuffer(rawChunk) ? rawChunk : Buffer.from(rawChunk);
    let start = 0;
    while (start < chunk.length) {
      const newline = chunk.indexOf(0x0a, start);
      const end = newline < 0 ? chunk.length : newline + 1;
      const piece = chunk.subarray(start, end);
      size += piece.length;
      if (size > maximumBytes) throw fail("JSONL line exceeds size limit", "JSONL_TOO_LARGE");
      pieces.push(piece);
      if (newline >= 0) {
        yield { bytes: Buffer.concat(pieces, size), terminated: true };
        pieces = [];
        size = 0;
      }
      start = end;
    }
  }
  if (size > 0) yield { bytes: Buffer.concat(pieces, size), terminated: false };
}

function imageIsReady(image) {
  return Boolean((typeof image.result === "string" && image.result.length) || image.savedPath);
}

export class SessionParser {
  constructor(currentTurnId) {
    this.threadId = undefined;
    this.currentTurnId = currentTurnId;
    this.images = new Map();
  }

  process(line, sessionPath) {
    let record;
    try {
      record = JSON.parse(Buffer.isBuffer(line) ? line.toString("utf8") : line);
    } catch {
      return;
    }
    const recordType = asString(record?.type);
    const payload = record?.payload;
    const payloadType = asString(payload?.type);
    if (recordType === "session_meta") {
      this.threadId = asString(payload?.id) ?? asString(payload?.session_id);
    } else if (recordType === "turn_context") {
      this.currentTurnId = asString(payload?.turn_id);
    } else if (recordType === "event_msg" && payloadType === "task_started") {
      this.currentTurnId = asString(payload?.turn_id);
    } else if (recordType === "response_item" && payloadType === "image_generation_call") {
      this.parseAndUpsert(payload, "id", sessionPath);
    } else if (recordType === "event_msg" && payloadType === "image_generation_end") {
      this.parseAndUpsert(payload, "call_id", sessionPath);
    } else if (
      recordType === "event_msg" &&
      payloadType === "task_complete" &&
      asString(payload?.turn_id) === this.currentTurnId
    ) {
      this.currentTurnId = undefined;
    }
  }

  parseAndUpsert(payload, idField, sessionPath) {
    const id = asString(payload?.[idField]);
    if (!id) return;
    let savedPath = asString(payload?.saved_path) ?? asString(payload?.savedPath);
    if (savedPath) {
      savedPath = path.isAbsolute(savedPath)
        ? savedPath
        : path.resolve(path.dirname(sessionPath), savedPath);
    }
    const image = {
      turnId:
        asString(getAt(payload, ["internal_chat_message_metadata_passthrough", "turn_id"])) ??
        asString(payload?.turn_id) ??
        this.currentTurnId,
      id,
      status: asString(payload?.status) ?? "unknown",
      revisedPrompt: asString(payload?.revised_prompt) ?? asString(payload?.revisedPrompt),
      result: asString(payload?.result) || undefined,
      savedPath: savedPath || undefined,
    };
    const existing = this.images.get(id);
    if (!existing) {
      this.images.set(id, image);
      return;
    }
    if (image.turnId) existing.turnId = image.turnId;
    if (image.status !== "unknown") existing.status = image.status;
    if (image.revisedPrompt) existing.revisedPrompt = image.revisedPrompt;
    if (image.result) existing.result = image.result;
    if (image.savedPath) existing.savedPath = image.savedPath;
  }

  snapshot() {
    return { threadId: this.threadId, images: [...this.images.values()].map((image) => ({ ...image })) };
  }
}

export class IncrementalSessionReader {
  constructor(sessionPath, { offset = 0, minimumOffset = offset, turnId } = {}) {
    this.path = sessionPath;
    this.offset = offset;
    this.minimumOffset = minimumOffset;
    this.parser = new SessionParser(turnId);
  }

  static fromStart(sessionPath) {
    return new IncrementalSessionReader(sessionPath, { offset: 0, minimumOffset: 0 });
  }

  static fromOffset(sessionPath, offset, turnId) {
    return new IncrementalSessionReader(sessionPath, { offset, minimumOffset: offset, turnId });
  }

  async refresh() {
    const metadata = await fs.stat(this.path);
    if (metadata.size < this.offset) {
      if (this.minimumOffset !== 0) {
        throw fail("Session was truncated after the turn started", "SESSION_TRUNCATED");
      }
      this.offset = 0;
      this.parser = new SessionParser();
    }
    const handle = await fs.open(this.path, "r");
    const stream = handle.createReadStream({ start: this.offset, autoClose: false });
    try {
      for await (const line of splitLines(stream)) {
        if (!line.terminated) break;
        this.offset += line.bytes.length;
        this.parser.process(line.bytes, this.path);
      }
    } finally {
      stream.destroy();
      await handle.close().catch(() => {});
    }
    return this.snapshot();
  }

  snapshot() {
    return this.parser.snapshot();
  }
}

async function existingJsonl(candidate) {
  if (!candidate || path.extname(candidate) !== ".jsonl") return undefined;
  try {
    const metadata = await fs.stat(candidate);
    return metadata.isFile() ? path.resolve(candidate) : undefined;
  } catch {
    return undefined;
  }
}

function pathIsWithin(candidate, directory) {
  const relative = path.relative(directory, candidate);
  return relative !== "" && !relative.startsWith(`..${path.sep}`) && relative !== ".." && !path.isAbsolute(relative);
}

async function allowedSessionPath(root, candidate) {
  const existing = await existingJsonl(candidate);
  if (!existing) return undefined;
  const canonical = await fs.realpath(existing).catch(() => undefined);
  if (!canonical) return undefined;
  for (const name of ["sessions", "archived_sessions"]) {
    const directory = await fs.realpath(path.join(root, name)).catch(() => undefined);
    if (directory && pathIsWithin(canonical, directory)) return canonical;
  }
  return undefined;
}

async function queryRolloutPath(databasePath, threadId) {
  let database;
  try {
    const { DatabaseSync } = await import("node:sqlite");
    database = new DatabaseSync(databasePath, { readOnly: true });
    database.exec("PRAGMA query_only=ON");
    database.exec("PRAGMA busy_timeout=200");
    const row = database
      .prepare("SELECT rollout_path FROM threads WHERE id = ? LIMIT 1")
      .get(threadId);
    return asString(row?.rollout_path);
  } catch {
    return undefined;
  } finally {
    try {
      database?.close();
    } catch {
      // The database is read-only and best-effort session discovery may continue.
    }
  }
}

async function findSessionInStateDatabases(root, threadId) {
  let entries;
  try {
    entries = await fs.readdir(root, { withFileTypes: true });
  } catch {
    return undefined;
  }
  const databases = [];
  for (const entry of entries) {
    if (!entry.isFile() || !/^state_.*\.sqlite$/.test(entry.name)) continue;
    const databasePath = path.join(root, entry.name);
    const metadata = await fs.stat(databasePath).catch(() => undefined);
    if (metadata) databases.push({ path: databasePath, modified: metadata.mtimeMs });
  }
  databases.sort((left, right) => right.modified - left.modified);
  for (const database of databases) {
    const rolloutPath = await queryRolloutPath(database.path, threadId);
    if (!rolloutPath) continue;
    const allowed = await allowedSessionPath(root, rolloutPath);
    if (allowed) return allowed;
  }
  return undefined;
}

async function findSessionByScan(directory, threadId) {
  const pending = [{ directory, depth: 0 }];
  let visited = 0;
  while (pending.length && visited < 20_000) {
    const current = pending.pop();
    let entries;
    try {
      entries = await fs.readdir(current.directory, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      visited += 1;
      if (visited > 20_000) break;
      const candidate = path.join(current.directory, entry.name);
      if (entry.isDirectory() && !entry.isSymbolicLink() && current.depth < 8) {
        pending.push({ directory: candidate, depth: current.depth + 1 });
      } else if (entry.isFile() && entry.name.endsWith(".jsonl") && entry.name.includes(threadId)) {
        return candidate;
      }
    }
  }
  return undefined;
}

export class SessionLocator {
  constructor(root = codexHome()) {
    this.root = root;
    this.cached = new Map();
  }

  remember(threadId, sessionPath) {
    this.cached.set(threadId, sessionPath);
  }

  async locateFast(threadId, hintedPath) {
    const hinted = await existingJsonl(hintedPath);
    if (hinted) {
      this.cached.set(threadId, hinted);
      return hinted;
    }
    const cached = await existingJsonl(this.cached.get(threadId));
    if (cached) return cached;
    const databasePath = await findSessionInStateDatabases(this.root, threadId);
    if (databasePath) this.cached.set(threadId, databasePath);
    return databasePath;
  }

  async locate(threadId, hintedPath) {
    const fast = await this.locateFast(threadId, hintedPath);
    if (fast) return fast;
    for (const name of ["sessions", "archived_sessions"]) {
      const candidate = await findSessionByScan(path.join(this.root, name), threadId);
      if (candidate) {
        this.cached.set(threadId, candidate);
        return candidate;
      }
    }
    return undefined;
  }
}

class SessionCache {
  constructor() {
    this.readers = new Map();
  }

  async read(sessionPath) {
    let reader = this.readers.get(sessionPath);
    if (!reader) {
      reader = IncrementalSessionReader.fromStart(sessionPath);
      this.readers.set(sessionPath, reader);
    }
    return reader.refresh();
  }
}

export class SeenCache {
  constructor(capacity = SEEN_CACHE_CAPACITY) {
    this.capacity = capacity;
    this.values = new Set();
    this.order = [];
  }

  has(value) {
    return this.values.has(value);
  }

  add(value) {
    if (this.values.has(value)) return;
    this.values.add(value);
    this.order.push(value);
    while (this.order.length > this.capacity) this.values.delete(this.order.shift());
  }
}

async function captureTurnStart(request, startedAtMs, sessions, knownThreadId) {
  const threadId = knownThreadId ?? threadIdFromTurnStart(request);
  if (!threadId) return undefined;
  const hintedPath =
    asString(getAt(request, ["params", "thread", "path"])) ??
    asString(getAt(request, ["params", "path"]));
  const sessionPath = await sessions.locateFast(threadId, hintedPath);
  const metadata = sessionPath ? await fs.stat(sessionPath).catch(() => undefined) : undefined;
  return {
    threadId,
    startedAtMs,
    sessionPath,
    sessionOffset: metadata?.size,
  };
}

function rememberPendingTurn(pendingTurns, start) {
  const cutoff = start.startedAtMs - TURN_GUARD_TTL_MS;
  for (const [threadId, queue] of pendingTurns) {
    while (queue[0]?.startedAtMs < cutoff) queue.shift();
    if (!queue.length) pendingTurns.delete(threadId);
  }
  const queue = pendingTurns.get(start.threadId) ?? [];
  queue.push(start);
  while (queue.length > 8) queue.shift();
  pendingTurns.set(start.threadId, queue);
}

function takePendingTurn(pendingTurns, threadId) {
  const queue = pendingTurns.get(threadId);
  if (!queue?.length) return undefined;
  const start = queue.shift();
  if (!queue.length) pendingTurns.delete(threadId);
  return start;
}

function completedItem(image, savedPath) {
  return {
    type: "imageGeneration",
    id: image.id,
    status: "completed",
    revisedPrompt: image.revisedPrompt ?? null,
    result: "",
    savedPath,
  };
}

async function materializeImageItem(outputRoot, threadId, turnId, item) {
  if (item?.type !== "imageGeneration" || typeof item.result !== "string" || !item.result) {
    return { changed: false };
  }
  if (typeof item.id !== "string" || !item.id) {
    throw fail("Official image item has no id", "INVALID_PROTOCOL");
  }
  const image = {
    turnId,
    id: item.id,
    status: asString(item.status) ?? "unknown",
    revisedPrompt: asString(item.revisedPrompt) ?? asString(item.revised_prompt),
    result: item.result,
    savedPath: asString(item.savedPath) ?? asString(item.saved_path),
  };
  const saved = await decodeAndSave(outputRoot, threadId, image);
  item.result = "";
  item.savedPath = saved.path;
  return { changed: true, sha256: saved.sha256 };
}

function containsImage(items, imageId) {
  return items.some((item) => item?.type === "imageGeneration" && item?.id === imageId);
}

function turnImages(snapshot, turnId) {
  return snapshot.images.filter((image) => image.turnId === turnId);
}

function turnHasPendingImage(snapshot, turnId) {
  const images = turnImages(snapshot, turnId);
  return images.length > 0 && images.some((image) => !imageIsReady(image));
}

async function loadTurnWithRetries(turnId, read, sleep = delay) {
  let snapshot = await read();
  if (!turnHasPendingImage(snapshot, turnId)) return snapshot;
  for (const milliseconds of RETRY_DELAYS_MS) {
    await sleep(milliseconds);
    snapshot = await read();
    if (!turnHasPendingImage(snapshot, turnId)) break;
  }
  return snapshot;
}

export class ResponseProcessor {
  constructor({
    requests = new Map(),
    sessions = new SessionLocator(),
    pendingTurns = new Map(),
    outputRoot = defaultOutputDirectory(),
    sleep = delay,
  } = {}) {
    this.requests = requests;
    this.sessions = sessions;
    this.pendingTurns = pendingTurns;
    this.history = new SessionCache();
    this.outputRoot = outputRoot;
    this.sleep = sleep;
    this.seen = new SeenCache();
    this.turnGuards = new Map();
  }

  async processLine(raw) {
    let value;
    try {
      value = JSON.parse(raw.toString("utf8"));
    } catch {
      return [raw];
    }
    const key = requestKey(value);
    const trackedRequest = key ? this.requests.get(key) : undefined;
    if (key) this.requests.delete(key);
    await this.observeTurnStart(value, trackedRequest);
    let changed = false;
    try {
      changed = (await this.sanitizeLiveImages(value)) > 0 || changed;
    } catch (error) {
      safeWarning("official image normalization skipped", error);
    }
    if (trackedRequest && isHistoryMethod(trackedRequest.method)) {
      try {
        changed = (await this.injectHistory(value)) > 0 || changed;
      } catch (error) {
        safeWarning("history injection skipped", error);
      }
    }
    this.observeOfficialImage(value);
    const output = [];
    if (value?.method === "turn/completed") {
      try {
        output.push(...(await this.injectRealtime(value)));
      } catch (error) {
        safeWarning("realtime injection skipped", error);
      }
    }
    output.push(changed ? Buffer.from(`${JSON.stringify(value)}\n`) : raw);
    return output;
  }

  async observeTurnStart(value, request) {
    if (request?.method === "turn/start") {
      const threadId = request.threadId;
      const turnId = responseTurnId(value);
      if (threadId && turnId) await this.registerTurnGuard(threadId, turnId);
    }
    if (value?.method !== "turn/started") return;
    const threadId =
      asString(getAt(value, ["params", "threadId"])) ??
      asString(getAt(value, ["params", "thread", "id"]));
    const turnId =
      asString(getAt(value, ["params", "turn", "id"])) ??
      asString(getAt(value, ["params", "turnId"]));
    if (threadId && turnId) await this.registerTurnGuard(threadId, turnId);
  }

  async registerTurnGuard(threadId, turnId) {
    const key = turnKey(threadId, turnId);
    if (this.turnGuards.has(key)) return;
    const start =
      takePendingTurn(this.pendingTurns, threadId) ??
      (await captureTurnStart({ params: { threadId } }, Date.now(), this.sessions, threadId));
    const reader =
      start?.sessionPath && Number.isSafeInteger(start.sessionOffset)
        ? IncrementalSessionReader.fromOffset(start.sessionPath, start.sessionOffset, turnId)
        : undefined;
    this.turnGuards.set(key, { startedAtMs: start?.startedAtMs ?? Date.now(), reader });
    const cutoff = Date.now() - TURN_GUARD_TTL_MS;
    for (const [guardKey, guard] of this.turnGuards) {
      if (guard.startedAtMs < cutoff) this.turnGuards.delete(guardKey);
    }
  }

  async sanitizeLiveImages(value) {
    const method = asString(value?.method);
    if (method === "item/started" || method === "item/completed") {
      const threadId = asString(getAt(value, ["params", "threadId"]));
      const turnId = asString(getAt(value, ["params", "turnId"]));
      if (!threadId || !turnId) throw fail("Image notification has no thread or turn", "INVALID_PROTOCOL");
      const item = getAt(value, ["params", "item"]);
      if (!item) return 0;
      const saved = await materializeImageItem(this.outputRoot, threadId, turnId, item);
      if (saved.sha256) this.seen.add(imageHashKey(threadId, turnId, saved.sha256));
      return Number(saved.changed);
    }
    if (method !== "turn/completed") return 0;
    const threadId = asString(getAt(value, ["params", "threadId"]));
    const turnId = asString(getAt(value, ["params", "turn", "id"]));
    const items = getAt(value, ["params", "turn", "items"]);
    if (!threadId || !turnId) throw fail("Completed turn has no thread or turn", "INVALID_PROTOCOL");
    if (!Array.isArray(items)) return 0;
    let changed = 0;
    for (const item of items) {
      const saved = await materializeImageItem(this.outputRoot, threadId, turnId, item);
      if (saved.changed) changed += 1;
      if (saved.sha256) this.seen.add(imageHashKey(threadId, turnId, saved.sha256));
    }
    return changed;
  }

  async injectHistory(response) {
    const thread = getAt(response, ["result", "thread"]);
    if (!thread || typeof thread !== "object") throw fail("History response has no thread", "INVALID_PROTOCOL");
    const threadId = asString(thread.id);
    if (!threadId) throw fail("History thread has no id", "INVALID_PROTOCOL");
    const sessionPath = await this.sessions.locate(threadId, asString(thread.path));
    if (!sessionPath) return 0;
    const snapshot = await this.history.read(sessionPath);
    if (!Array.isArray(thread.turns)) return 0;
    let injected = 0;
    for (const turn of thread.turns) {
      const turnId = asString(turn?.id);
      const items = turn?.items;
      if (!turnId || !Array.isArray(items)) continue;
      const hashes = new Set();
      for (const item of items) {
        try {
          const saved = await materializeImageItem(this.outputRoot, threadId, turnId, item);
          if (saved.changed) injected += 1;
          if (saved.sha256) hashes.add(saved.sha256);
        } catch (error) {
          safeWarning("official history image normalization skipped", error);
        }
      }
      for (const image of turnImages(snapshot, turnId).filter(imageIsReady)) {
        if (containsImage(items, image.id)) continue;
        let saved;
        try {
          saved = await decodeAndSave(this.outputRoot, threadId, image);
        } catch (error) {
          safeWarning("history image skipped", error);
          continue;
        }
        if (hashes.has(saved.sha256)) continue;
        hashes.add(saved.sha256);
        let position = items.length;
        for (let index = items.length - 1; index >= 0; index -= 1) {
          if (items[index]?.type === "agentMessage") {
            position = index;
            break;
          }
        }
        items.splice(position, 0, completedItem(image, saved.path));
        injected += 1;
      }
    }
    return injected;
  }

  async injectRealtime(notification) {
    const threadId = asString(getAt(notification, ["params", "threadId"]));
    const turnId = asString(getAt(notification, ["params", "turn", "id"]));
    if (!threadId || !turnId) throw fail("Completed turn has no thread or turn", "INVALID_PROTOCOL");
    const officialIds = new Set(
      (getAt(notification, ["params", "turn", "items"]) ?? [])
        .filter((item) => item?.type === "imageGeneration")
        .map((item) => asString(item.id))
        .filter(Boolean),
    );
    const key = turnKey(threadId, turnId);
    const guard = this.turnGuards.get(key);
    this.turnGuards.delete(key);
    const startedAtMs = guard?.startedAtMs ?? Date.now();
    let snapshot;
    if (guard?.reader) {
      snapshot = await loadTurnWithRetries(
        turnId,
        () => guard.reader.refresh(),
        this.sleep,
      );
    } else {
      const sessionPath = await this.sessions.locate(threadId);
      if (!sessionPath) return [];
      snapshot = await loadTurnWithRetries(
        turnId,
        () => this.history.read(sessionPath),
        this.sleep,
      );
    }
    const output = [];
    for (const image of turnImages(snapshot, turnId).filter(imageIsReady)) {
      const idKey = imageIdKey(threadId, turnId, image.id);
      if (officialIds.has(image.id) || this.seen.has(idKey)) continue;
      let saved;
      try {
        saved = await decodeAndSave(this.outputRoot, threadId, image);
      } catch (error) {
        safeWarning("realtime image skipped", error);
        continue;
      }
      const hashKey = imageHashKey(threadId, turnId, saved.sha256);
      if (this.seen.has(hashKey)) continue;
      output.push(
        Buffer.from(
          `${JSON.stringify({
            method: "item/started",
            params: {
              threadId,
              turnId,
              item: {
                type: "imageGeneration",
                id: image.id,
                status: "in_progress",
                revisedPrompt: null,
                result: "",
              },
              startedAtMs,
            },
          })}\n`,
        ),
      );
      output.push(
        Buffer.from(
          `${JSON.stringify({
            method: "item/completed",
            params: {
              threadId,
              turnId,
              item: completedItem(image, saved.path),
              completedAtMs: Date.now(),
            },
          })}\n`,
        ),
      );
      this.seen.add(idKey);
      this.seen.add(hashKey);
    }
    return output;
  }

  observeOfficialImage(value) {
    if (!["item/started", "item/completed"].includes(value?.method)) return;
    const item = getAt(value, ["params", "item"]);
    const threadId = asString(getAt(value, ["params", "threadId"]));
    const turnId = asString(getAt(value, ["params", "turnId"]));
    const imageId = asString(item?.id);
    if (item?.type === "imageGeneration" && threadId && turnId && imageId) {
      this.seen.add(imageIdKey(threadId, turnId, imageId));
    }
  }
}

async function writeStream(stream, bytes) {
  if (stream.write(bytes)) return;
  await once(stream, "drain");
}

async function pumpRequests(input, childInput, state) {
  let firstLine = true;
  for await (const line of splitLines(input)) {
    const receivedAtMs = Date.now();
    let forwarded = line.bytes;
    if (firstLine) {
      firstLine = false;
      if (forwarded.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf]))) {
        forwarded = forwarded.subarray(3);
      }
    }
    try {
      const value = JSON.parse(forwarded.toString("utf8"));
      const key = requestKey(value);
      const method = asString(value?.method);
      if (key && method) {
        const threadId = threadIdFromTurnStart(value);
        if (method === "turn/start") {
          const start = await captureTurnStart(value, receivedAtMs, state.sessions, threadId);
          if (start) rememberPendingTurn(state.pendingTurns, start);
        }
        state.requests.set(key, { method, threadId });
      }
    } catch {
      // Non-JSON protocol lines are forwarded byte-for-byte.
    }
    await writeStream(childInput, forwarded);
  }
  childInput.end();
}

async function pumpResponses(input, output, processor) {
  for await (const line of splitLines(input)) {
    const messages = await processor.processLine(line.bytes);
    for (const message of messages) await writeStream(output, message);
  }
}

export function normalizeBaseUrl(input) {
  let url;
  try {
    url = new URL(String(input).trim());
  } catch {
    throw fail("API 地址格式无效", "INVALID_BASE_URL");
  }
  if (!url.hostname || url.username || url.password || url.search || url.hash) {
    throw fail("API 地址不能包含账号、查询参数或片段", "INVALID_BASE_URL");
  }
  if (/%2f|%5c/i.test(url.pathname)) {
    throw fail("API 地址路径不能包含编码后的分隔符", "INVALID_BASE_URL");
  }
  const local = url.hostname === "localhost" || url.hostname === "127.0.0.1" || url.hostname === "[::1]" || url.hostname === "::1";
  if (url.protocol !== "https:" && !(local && url.protocol === "http:")) {
    throw fail("远程 API 地址必须使用 HTTPS", "INVALID_BASE_URL");
  }
  let pathname = url.pathname.replace(/\/+$/, "");
  const v1Segments = pathname
    .split("/")
    .filter((segment) => segment.toLowerCase() === "v1").length;
  if (v1Segments === 0) pathname = `${pathname || ""}/v1`;
  else if (v1Segments !== 1 || !pathname.toLowerCase().endsWith("/v1")) {
    throw fail("API 地址中的 /v1 路径重复", "INVALID_BASE_URL");
  }
  url.pathname = pathname;
  return url.toString().replace(/\/$/, "");
}

function tomlString(value) {
  return JSON.stringify(value);
}

export function buildCodexArgs(originalArgs, baseUrl, imageEnabled = true) {
  const normalized = normalizeBaseUrl(baseUrl);
  const overrides = [
    ["model_provider", "comidea"],
    ["model_providers.comidea.name", "OpenAI"],
    ["model_providers.comidea.base_url", normalized],
    ["model_providers.comidea.env_key", "COMIDEA_API_KEY"],
    ["model_providers.comidea.wire_api", "responses"],
  ];
  const args = [];
  for (const [key, value] of overrides) args.push("-c", `${key}=${tomlString(value)}`);
  args.push("-c", "model_providers.comidea.requires_openai_auth=true");
  args.push("-c", `features.image_generation=${imageEnabled ? "true" : "false"}`);
  return [...args, ...originalArgs];
}

export function createChildEnvironment(environment = process.env) {
  const childEnvironment = { ...environment };
  for (const variable of BRIDGE_ENVIRONMENT_VARIABLES) delete childEnvironment[variable];
  return childEnvironment;
}

async function isExecutable(filePath) {
  try {
    const metadata = await fs.stat(filePath);
    await fs.access(filePath, fsConstants.X_OK);
    return metadata.isFile();
  } catch {
    return false;
  }
}

function plistValue(infoPath, key) {
  const result = spawnSync(
    "/usr/bin/plutil",
    ["-extract", key, "raw", "-o", "-", infoPath],
    { encoding: "utf8", timeout: 3000, windowsHide: true },
  );
  return result.status === 0 ? result.stdout.trim() : undefined;
}

export async function findCodexInstall({ platform = process.platform, home = os.homedir() } = {}) {
  if (platform !== "darwin") return undefined;
  const bundles = [
    "/Applications/Codex.app",
    "/Applications/ChatGPT.app",
    path.join(home, "Applications", "Codex.app"),
    path.join(home, "Applications", "ChatGPT.app"),
  ];
  for (const bundle of bundles) {
    const infoPath = path.join(bundle, "Contents", "Info.plist");
    const realCli = path.join(bundle, "Contents", "Resources", "codex");
    if (!(await isExecutable(realCli))) continue;
    const executableName =
      plistValue(infoPath, "CFBundleExecutable") ?? path.basename(bundle, ".app");
    const appExecutable = path.join(bundle, "Contents", "MacOS", executableName);
    if (!(await isExecutable(appExecutable))) continue;
    const bundleId = plistValue(infoPath, "CFBundleIdentifier");
    return { bundle, appExecutable, realCli, bundleId };
  }
  return undefined;
}

async function resolveRealCli() {
  const configured = process.env.CODEX_IMAGE_BRIDGE_REAL_CLI;
  if (configured && (await isExecutable(configured))) return configured;
  const install = await findCodexInstall();
  if (!install) throw fail("未找到 Codex.app 或其内置 CLI", "CODEX_NOT_FOUND");
  return install.realCli;
}

function spawnChild(realCli, args) {
  return spawn(realCli, args, {
    env: createChildEnvironment(),
    stdio: ["pipe", "pipe", "inherit"],
    windowsHide: true,
  });
}

async function childExit(child) {
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
}

async function runAppServer(realCli, args) {
  const child = spawnChild(realCli, args);
  const state = {
    requests: new Map(),
    sessions: new SessionLocator(),
    pendingTurns: new Map(),
  };
  const processor = new ResponseProcessor(state);
  let forwardingError;
  let ending = false;
  const requests = pumpRequests(process.stdin, child.stdin, state).catch((error) => {
    if (!ending) {
      forwardingError = error;
      child.kill();
    }
  });
  const responses = pumpResponses(child.stdout, process.stdout, processor);
  const relayInterrupt = () => child.kill("SIGINT");
  const relayTermination = () => child.kill("SIGTERM");
  process.once("SIGINT", relayInterrupt);
  process.once("SIGTERM", relayTermination);
  try {
    const status = await childExit(child);
    ending = true;
    process.stdin.pause();
    process.stdin.destroy();
    child.stdin.destroy();
    await responses;
    if (forwardingError) throw forwardingError;
    if (child.stdin.writableEnded) await requests;
    if (status.signal) return 1;
    return status.code ?? 1;
  } finally {
    process.removeListener("SIGINT", relayInterrupt);
    process.removeListener("SIGTERM", relayTermination);
  }
}

async function runPassthrough(realCli, args) {
  const child = spawn(realCli, args, {
    env: createChildEnvironment(),
    stdio: "inherit",
    windowsHide: true,
  });
  const status = await childExit(child);
  return status.signal ? 1 : (status.code ?? 1);
}

export async function runCliProxy(originalArgs) {
  const realCli = await resolveRealCli();
  const enabled = process.env.CODEX_IMAGE_BRIDGE_ENABLED === "1";
  let args = [...originalArgs];
  if (enabled) {
    const baseUrl = process.env.CODEX_IMAGE_BRIDGE_BASE_URL;
    const apiKey = process.env.COMIDEA_API_KEY;
    if (!baseUrl || !apiKey) throw fail("桥接进程缺少 API 配置", "CONFIG_MISSING");
    args = buildCodexArgs(
      args,
      baseUrl,
      process.env.CODEX_IMAGE_BRIDGE_IMAGE_ENABLED !== "0",
    );
  }
  return args.includes("app-server")
    ? runAppServer(realCli, args)
    : runPassthrough(realCli, args);
}

function requireNode24() {
  const major = Number.parseInt(process.versions.node.split(".")[0], 10);
  if (major < 24) throw fail("此版本需要 Node.js 24 或更高版本", "NODE_VERSION");
}

export async function probeSystem() {
  const install = await findCodexInstall();
  return {
    product: "Comidea Codex Image Bridge",
    website: "comidea.org",
    platform: process.platform,
    architecture: process.arch,
    node: process.versions.node,
    nodeSupported: Number.parseInt(process.versions.node.split(".")[0], 10) >= 24,
    codexHome: codexHome(),
    codexFound: Boolean(install),
    appBundle: install?.bundle ?? null,
    bundledCli: install?.realCli ?? null,
    bridgeScript: await fs.realpath(MODULE_PATH).catch(() => MODULE_PATH),
    imageModel: IMAGE_MODEL,
  };
}

export function buildLaunchEnvironment(config, install, scriptPath, environment = process.env) {
  return {
    ...environment,
    CODEX_CLI_PATH: scriptPath,
    CODEX_IMAGE_BRIDGE_REAL_CLI: install.realCli,
    CODEX_IMAGE_BRIDGE_BASE_URL: config.baseUrl,
    CODEX_IMAGE_BRIDGE_IMAGE_ENABLED: config.imageEnabled ? "1" : "0",
    CODEX_IMAGE_BRIDGE_ENABLED: "1",
    COMIDEA_API_KEY: config.apiKey,
    NODE_NO_WARNINGS: "1",
  };
}

function processMatchesExecutable(appExecutable) {
  const pattern = appExecutable.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const result = spawnSync("/usr/bin/pgrep", ["-f", pattern], {
    encoding: "utf8",
    timeout: 3000,
    windowsHide: true,
  });
  return result.status === 0 && result.stdout.trim().length > 0;
}

async function quitCodex(bundleId, appExecutable) {
  if (bundleId && /^[A-Za-z0-9.-]+$/.test(bundleId)) {
    spawnSync(
      "/usr/bin/osascript",
      ["-e", `tell application id "${bundleId}" to quit`],
      { encoding: "utf8", timeout: 5000, windowsHide: true },
    );
  }
  for (let attempt = 0; attempt < 40; attempt += 1) {
    if (!processMatchesExecutable(appExecutable)) return;
    await delay(250);
  }
  throw fail("Codex 未完全退出，请手动退出后重试", "CODEX_STILL_RUNNING");
}

export async function launchCodex(config) {
  requireNode24();
  if (process.platform !== "darwin") throw fail("启动功能只支持 macOS", "UNSUPPORTED_PLATFORM");
  const install = await findCodexInstall();
  if (!install) throw fail("未找到 Codex.app，请先安装 Codex Desktop", "CODEX_NOT_FOUND");
  const scriptPath = await fs.realpath(MODULE_PATH);
  await fs.chmod(scriptPath, 0o700);
  await quitCodex(install.bundleId, install.appExecutable);
  const child = spawn(install.appExecutable, [], {
    detached: true,
    env: buildLaunchEnvironment(config, install, scriptPath),
    stdio: "ignore",
    windowsHide: true,
  });
  child.unref();
  return { launched: true, pid: child.pid, appBundle: install.bundle };
}

async function readResponseText(response, limit = MAX_HTTP_RESPONSE_BYTES) {
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) {
    throw fail("服务响应过大", "HTTP_RESPONSE_TOO_LARGE");
  }
  if (!response.body) return "";
  const chunks = [];
  let size = 0;
  for await (const chunk of response.body) {
    const bytes = Buffer.from(chunk);
    size += bytes.length;
    if (size > limit) throw fail("服务响应过大", "HTTP_RESPONSE_TOO_LARGE");
    chunks.push(bytes);
  }
  return Buffer.concat(chunks, size).toString("utf8");
}

export async function testApiConnection(config) {
  const baseUrl = normalizeBaseUrl(config.baseUrl);
  if (!config.apiKey) throw fail("请先输入 API Key", "API_KEY_MISSING");
  let response;
  try {
    response = await fetch(`${baseUrl}/models`, {
      headers: { Authorization: `Bearer ${config.apiKey}`, Accept: "application/json" },
      redirect: "error",
      signal: AbortSignal.timeout(12_000),
    });
  } catch {
    throw fail("连接失败，请检查地址、网络、DNS 和 TLS", "NETWORK_ERROR");
  }
  if (response.status === 401 || response.status === 403) {
    await response.body?.cancel().catch(() => {});
    throw fail("服务拒绝了 API Key", "AUTH_FAILED");
  }
  if (response.status === 404 || response.status === 405) {
    await response.body?.cancel().catch(() => {});
    return { reachable: true, authenticated: null, modelFound: null, imageModel: IMAGE_MODEL };
  }
  if (!response.ok) {
    await response.body?.cancel().catch(() => {});
    throw fail(`服务返回 HTTP ${response.status}`, "HTTP_ERROR");
  }
  const text = await readResponseText(response);
  let payload;
  try {
    payload = JSON.parse(text);
  } catch {
    throw fail("服务的 /models 响应不是有效 JSON", "INVALID_RESPONSE");
  }
  const models = Array.isArray(payload?.data) ? payload.data : [];
  return {
    reachable: true,
    authenticated: true,
    modelFound: models.some((model) => model?.id === IMAGE_MODEL),
    imageModel: IMAGE_MODEL,
  };
}

function secureEqual(left, right) {
  if (typeof left !== "string" || typeof right !== "string") return false;
  const leftBytes = Buffer.from(left);
  const rightBytes = Buffer.from(right);
  return leftBytes.length === rightBytes.length && timingSafeEqual(leftBytes, rightBytes);
}

function parseCookies(header) {
  const cookies = new Map();
  for (const part of String(header ?? "").split(";")) {
    const separator = part.indexOf("=");
    if (separator <= 0) continue;
    cookies.set(part.slice(0, separator).trim(), part.slice(separator + 1).trim());
  }
  return cookies;
}

export function isAllowedHost(host, port) {
  return host === `127.0.0.1:${port}`;
}

function securityHeaders(nonce) {
  return {
    "Cache-Control": "no-store",
    "Content-Security-Policy": [
      "default-src 'none'",
      `style-src 'nonce-${nonce}'`,
      `script-src 'nonce-${nonce}'`,
      "connect-src 'self'",
      "img-src 'self' data:",
      "base-uri 'none'",
      "form-action 'self'",
      "frame-ancestors 'none'",
    ].join("; "),
    "Cross-Origin-Opener-Policy": "same-origin",
    "Referrer-Policy": "no-referrer",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
  };
}

function sendJson(response, status, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, {
    "Cache-Control": "no-store",
    "Content-Type": "application/json; charset=utf-8",
    "Content-Length": body.length,
    "X-Content-Type-Options": "nosniff",
  });
  response.end(body);
}

function publicError(error) {
  const known = new Set([
    "INVALID_BASE_URL",
    "API_KEY_MISSING",
    "NETWORK_ERROR",
    "AUTH_FAILED",
    "HTTP_ERROR",
    "HTTP_RESPONSE_TOO_LARGE",
    "INVALID_RESPONSE",
    "CODEX_NOT_FOUND",
    "NODE_VERSION",
    "UNSUPPORTED_PLATFORM",
    "CODEX_STILL_RUNNING",
    "UNSUPPORTED_CONTENT_TYPE",
    "INVALID_JSON",
    "HTTP_BODY_TOO_LARGE",
  ]);
  if (known.has(error?.code)) return error.message;
  return "操作失败，请查看诊断状态后重试";
}

async function readJsonRequest(request) {
  const contentType = String(request.headers["content-type"] ?? "").toLowerCase();
  if (!contentType.startsWith("application/json")) {
    throw fail("请求必须使用 application/json", "UNSUPPORTED_CONTENT_TYPE");
  }
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > MAX_HTTP_BODY_BYTES) throw fail("请求体过大", "HTTP_BODY_TOO_LARGE");
    chunks.push(chunk);
  }
  try {
    return JSON.parse(Buffer.concat(chunks, size).toString("utf8") || "{}");
  } catch {
    throw fail("请求 JSON 无效", "INVALID_JSON");
  }
}

function renderUi(nonce, csrfToken) {
  return `<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <meta name="csrf-token" content="${csrfToken}">
  <title>Comidea Codex Image Bridge</title>
  <style nonce="${nonce}">
    :root{color-scheme:light;--ink:#171a1f;--muted:#66707a;--line:#d9dee3;--paper:#fff;--wash:#f3f5f6;--side:#16191d;--side2:#22272d;--green:#1f9d68;--green2:#14784e;--red:#bc3f45;--amber:#9a6a18;letter-spacing:0}
    *{box-sizing:border-box;letter-spacing:0}html,body{margin:0;min-height:100%;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:var(--ink);background:var(--wash)}
    button,input{font:inherit}button{cursor:pointer}.app{min-height:100vh;display:grid;grid-template-columns:238px minmax(0,1fr)}
    aside{background:var(--side);color:#edf1f3;display:flex;flex-direction:column;min-height:100vh;border-right:1px solid #0d0f11}
    .brand{padding:25px 22px 22px;border-bottom:1px solid #2a2f35}.brand-row{display:flex;align-items:center;gap:12px}.mark{width:30px;height:30px;display:grid;grid-template-columns:1fr 1fr;gap:3px}.mark i{display:block;background:#fff}.mark i:last-child{background:var(--green)}
    .brand strong{display:block;font-size:14px}.brand small{display:block;margin-top:3px;color:#9fa8b1;font-size:11px;text-transform:uppercase}
    nav{padding:14px 10px;display:grid;gap:4px}.nav{position:relative;width:100%;height:43px;border:0;background:transparent;color:#aeb6be;text-align:left;padding:0 16px;border-radius:5px}.nav:hover{background:#1e2328;color:#fff}.nav.active{background:var(--side2);color:#fff}.nav.active:before{content:"";position:absolute;left:0;top:9px;bottom:9px;width:3px;background:var(--green)}
    .side-foot{margin-top:auto;padding:18px 22px;border-top:1px solid #2a2f35;color:#89939d;font-size:12px}.side-foot b{display:block;color:#d6dce1;margin-bottom:4px}
    main{min-width:0;padding:31px 40px 48px}.top{max-width:920px;display:flex;align-items:flex-start;justify-content:space-between;gap:24px;margin-bottom:28px}.top h1{font-size:24px;line-height:1.25;margin:0 0 7px}.top p{margin:0;color:var(--muted);font-size:14px}.pill{flex:0 0 auto;border:1px solid var(--line);background:var(--paper);padding:8px 11px;border-radius:5px;font-size:12px;color:var(--muted)}
    .page{display:none;max-width:920px}.page.active{display:block}.section{background:var(--paper);border:1px solid var(--line);border-radius:7px;margin-bottom:16px}.section-head{padding:18px 20px 14px;border-bottom:1px solid #e7eaed}.section-head h2{font-size:15px;margin:0 0 5px}.section-head p{font-size:13px;color:var(--muted);margin:0}.section-body{padding:20px}
    .status-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:0;border-top:1px solid #e7eaed}.status{padding:17px 20px;border-right:1px solid #e7eaed;border-bottom:1px solid #e7eaed;min-width:0}.status:nth-child(2n){border-right:0}.status:nth-last-child(-n+2){border-bottom:0}.status label{display:block;color:var(--muted);font-size:12px;margin-bottom:7px}.status strong{display:block;font-size:14px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.ok{color:var(--green2)}.bad{color:var(--red)}.warn{color:var(--amber)}
    .field{margin-bottom:18px}.field:last-child{margin-bottom:0}.field label{display:block;font-weight:650;font-size:13px;margin-bottom:7px}.field-note{display:block;color:var(--muted);font-size:12px;margin-top:7px;line-height:1.45}.input{width:100%;height:43px;border:1px solid #bfc7ce;background:#fff;border-radius:5px;padding:0 12px;color:var(--ink);outline:0}.input:hover{border-color:#929ca5}.input:focus{border-color:var(--green);box-shadow:0 0 0 3px rgba(31,157,104,.13)}.input::placeholder{color:#9ca5ad}
    .switch-row{display:flex;align-items:center;justify-content:space-between;gap:20px;border-top:1px solid #e7eaed;padding-top:18px}.switch-copy strong{display:block;font-size:13px}.switch-copy span{display:block;color:var(--muted);font-size:12px;margin-top:4px}.toggle{position:relative;width:42px;height:24px;flex:0 0 auto}.toggle input{position:absolute;opacity:0}.toggle span{position:absolute;inset:0;border-radius:12px;background:#aab2b9;transition:background .15s}.toggle span:after{content:"";position:absolute;width:18px;height:18px;left:3px;top:3px;border-radius:50%;background:#fff;transition:transform .15s;box-shadow:0 1px 3px rgba(0,0,0,.25)}.toggle input:checked+span{background:var(--green)}.toggle input:checked+span:after{transform:translateX(18px)}.toggle input:focus-visible+span{outline:2px solid var(--green);outline-offset:2px}
    .check{display:flex;align-items:center;gap:8px;color:var(--muted);font-size:12px;margin-top:9px}.check input{accent-color:var(--green)}.actions{display:flex;flex-wrap:wrap;gap:9px;padding:16px 20px;border-top:1px solid #e7eaed}.btn{min-height:39px;border-radius:5px;padding:0 15px;border:1px solid #b8c0c7;background:#fff;color:var(--ink);font-weight:650;font-size:13px}.btn:hover{background:#f3f5f6}.btn.primary{background:var(--green);border-color:var(--green);color:#fff}.btn.primary:hover{background:var(--green2)}.btn:disabled{opacity:.55;cursor:not-allowed}.feedback{min-height:20px;margin:0 20px 18px;font-size:13px;color:var(--muted)}.feedback.success{color:var(--green2)}.feedback.error{color:var(--red)}
    pre{margin:0;white-space:pre-wrap;overflow-wrap:anywhere;font:12px/1.65 ui-monospace,SFMono-Regular,Menlo,monospace;color:#2e353b}.diagnostic-actions{padding:0 20px 20px;display:flex;gap:9px}.model-line{display:flex;align-items:center;justify-content:space-between;gap:20px}.model-id{font:650 14px ui-monospace,SFMono-Regular,Menlo,monospace}.tag{border:1px solid #b8dfce;background:#edf8f3;color:var(--green2);border-radius:4px;padding:4px 7px;font-size:11px}
    @media(max-width:760px){.app{display:block}aside{min-height:auto}.brand{padding:17px 18px}.side-foot{display:none}nav{grid-template-columns:repeat(3,1fr);padding:8px}.nav{text-align:center;padding:0 7px}.nav.active:before{left:12px;right:12px;top:auto;bottom:0;width:auto;height:3px}main{padding:23px 16px 38px}.top{display:block;margin-bottom:20px}.pill{display:inline-block;margin-top:14px}.status-grid{grid-template-columns:1fr}.status,.status:nth-child(2n){border-right:0}.status:nth-last-child(-n+2){border-bottom:1px solid #e7eaed}.status:last-child{border-bottom:0}.actions .btn{flex:1 1 140px}}
  </style>
</head>
<body>
<div class="app">
  <aside>
    <div class="brand"><div class="brand-row"><span class="mark"><i></i><i></i><i></i><i></i></span><span><strong>Codex Image Bridge</strong><small>comidea.org</small></span></div></div>
    <nav aria-label="功能菜单">
      <button class="nav active" data-page="overview">概览</button>
      <button class="nav" data-page="service">模型服务</button>
      <button class="nav" data-page="diagnostics">诊断</button>
    </nav>
    <div class="side-foot"><b>macOS 临时版</b>Node.js 24 · 单文件运行</div>
  </aside>
  <main>
    <header class="top"><div><h1 id="page-title">运行概览</h1><p id="page-subtitle">检查本机环境与桥接状态</p></div><span class="pill" id="runtime-pill">正在检测</span></header>
    <section class="page active" id="page-overview">
      <div class="section">
        <div class="section-head"><h2>本机状态</h2><p>Codex Desktop 必须通过本工具重新启动</p></div>
        <div class="status-grid">
          <div class="status"><label>操作系统</label><strong id="status-platform">-</strong></div>
          <div class="status"><label>Node.js</label><strong id="status-node">-</strong></div>
          <div class="status"><label>Codex Desktop</label><strong id="status-codex">-</strong></div>
          <div class="status"><label>模型配置</label><strong id="status-config">未保存</strong></div>
        </div>
      </div>
      <div class="section"><div class="section-head"><h2>图片能力</h2><p>当前 Codex 图片工具使用固定后端模型</p></div><div class="section-body model-line"><span class="model-id">gpt-image-2</span><span class="tag">IMAGE TOOL</span></div></div>
    </section>
    <section class="page" id="page-service">
      <div class="section">
        <div class="section-head"><h2>OpenAI 兼容服务</h2><p>API Key 仅保存在本次 Node 进程内存中</p></div>
        <div class="section-body">
          <div class="field"><label for="base-url">服务器地址</label><input class="input" id="base-url" type="url" autocomplete="url" spellcheck="false" placeholder="https://api.example.com/v1"><span class="field-note">远程地址使用 HTTPS；末尾 /v1 可自动补全</span></div>
          <div class="field"><label for="api-key">API Key</label><input class="input" id="api-key" type="password" autocomplete="off" spellcheck="false" placeholder="输入本次启动使用的 API Key"><label class="check"><input id="show-key" type="checkbox">显示输入内容</label></div>
          <div class="switch-row"><div class="switch-copy"><strong>启用图片生成</strong><span>为 Codex 开启 gpt-image-2 图片工具</span></div><label class="toggle"><input id="image-enabled" type="checkbox" checked><span></span></label></div>
        </div>
        <div class="actions"><button class="btn" id="save">保存本次设置</button><button class="btn" id="test">测试连接</button><button class="btn primary" id="launch">启动 Codex</button></div>
        <p class="feedback" id="feedback" role="status"></p>
      </div>
    </section>
    <section class="page" id="page-diagnostics">
      <div class="section"><div class="section-head"><h2>脱敏诊断</h2><p>不包含 API Key、会话正文、提示词或 Base64</p></div><div class="section-body"><pre id="diagnostic-json">正在读取...</pre></div><div class="diagnostic-actions"><button class="btn" id="refresh">刷新检测</button><button class="btn" id="copy">复制诊断</button></div></div>
    </section>
  </main>
</div>
<script nonce="${nonce}">
  const csrf = document.querySelector('meta[name="csrf-token"]').content;
  const titles = {overview:['运行概览','检查本机环境与桥接状态'],service:['模型服务','配置本次 Codex 启动使用的服务'],diagnostics:['诊断','查看不含敏感信息的本机检测结果']};
  const feedback = document.getElementById('feedback');
  let latestStatus = null;
  document.querySelector('nav').addEventListener('click', function(event){
    const button = event.target.closest('[data-page]'); if(!button) return;
    const page = button.dataset.page;
    document.querySelectorAll('.nav').forEach(function(item){item.classList.toggle('active',item===button)});
    document.querySelectorAll('.page').forEach(function(item){item.classList.toggle('active',item.id==='page-'+page)});
    document.getElementById('page-title').textContent=titles[page][0];
    document.getElementById('page-subtitle').textContent=titles[page][1];
  });
  async function api(route, method, body){
    const options={method:method||'GET',headers:{'Accept':'application/json'}};
    if(options.method==='POST'){options.headers['Content-Type']='application/json';options.headers['X-CSRF-Token']=csrf;options.body=JSON.stringify(body||{})}
    const response=await fetch(route,options); const payload=await response.json();
    if(!response.ok) throw new Error(payload.error||'操作失败'); return payload;
  }
  function setBusy(busy){document.querySelectorAll('.actions button').forEach(function(button){button.disabled=busy})}
  function message(text,kind){feedback.textContent=text;feedback.className='feedback '+(kind||'')}
  function updateStatus(status){
    latestStatus=status; const probe=status.probe;
    document.getElementById('status-platform').textContent=probe.platform+' / '+probe.architecture;
    const node=document.getElementById('status-node');node.textContent='v'+probe.node;node.className=probe.nodeSupported?'ok':'bad';
    const codex=document.getElementById('status-codex');codex.textContent=probe.codexFound?'已找到':'未找到';codex.className=probe.codexFound?'ok':'bad';
    const config=document.getElementById('status-config');config.textContent=status.configured?'本次运行已保存':'未保存';config.className=status.configured?'ok':'warn';
    const pill=document.getElementById('runtime-pill');pill.textContent=probe.codexFound&&probe.nodeSupported?'环境就绪':'需要处理';
    document.getElementById('diagnostic-json').textContent=JSON.stringify(probe,null,2);
    if(status.baseUrl&&!document.getElementById('base-url').value)document.getElementById('base-url').value=status.baseUrl;
    if(status.hasApiKey)document.getElementById('api-key').placeholder='本次运行已保存，可留空复用';
  }
  async function refresh(){try{updateStatus(await api('/api/status'))}catch(error){document.getElementById('diagnostic-json').textContent=error.message}}
  async function saveConfig(){
    const payload={baseUrl:document.getElementById('base-url').value,apiKey:document.getElementById('api-key').value,imageEnabled:document.getElementById('image-enabled').checked};
    const saved=await api('/api/save','POST',payload);document.getElementById('api-key').value='';await refresh();return saved;
  }
  document.getElementById('show-key').addEventListener('change',function(){document.getElementById('api-key').type=this.checked?'text':'password'});
  document.getElementById('save').addEventListener('click',async function(){setBusy(true);message('正在保存...');try{await saveConfig();message('设置已保存在本次运行内存中','success')}catch(error){message(error.message,'error')}finally{setBusy(false)}});
  document.getElementById('test').addEventListener('click',async function(){setBusy(true);message('正在检查服务...');try{await saveConfig();const result=await api('/api/test','POST',{});const text=result.modelFound===true?'连接、鉴权与模型检查成功':result.modelFound===false?'连接与鉴权成功，但 /models 未列出 gpt-image-2':'服务器可达；/models 未提供，鉴权与模型待实际请求确认';message(text,'success')}catch(error){message(error.message,'error')}finally{setBusy(false)}});
  document.getElementById('launch').addEventListener('click',async function(){setBusy(true);message('正在重新启动 Codex...');try{await saveConfig();await api('/api/launch','POST',{});message('Codex 已启动，本工具会自动退出','success')}catch(error){message(error.message,'error');setBusy(false)}});
  document.getElementById('refresh').addEventListener('click',refresh);
  document.getElementById('copy').addEventListener('click',async function(){if(latestStatus)await navigator.clipboard.writeText(JSON.stringify(latestStatus.probe,null,2))});
  refresh();
</script>
</body>
</html>`;
}

function openDefaultBrowser(url) {
  let command;
  let args;
  if (process.platform === "darwin") {
    command = "/usr/bin/open";
    args = [url];
  } else if (process.platform === "win32") {
    command = "rundll32.exe";
    args = ["url.dll,FileProtocolHandler", url];
  } else {
    command = "xdg-open";
    args = [url];
  }
  const child = spawn(command, args, { detached: true, stdio: "ignore", windowsHide: true });
  child.unref();
}

export async function startUiServer({
  openBrowser = true,
  launch = launchCodex,
  probe = probeSystem,
  testConnection = testApiConnection,
  shutdownAfterLaunch = true,
} = {}) {
  const startupToken = randomBytes(32).toString("base64url");
  const sessionToken = randomBytes(32).toString("base64url");
  const csrfToken = randomBytes(32).toString("base64url");
  let runtimeConfig = { baseUrl: "", apiKey: "", imageEnabled: true };
  let port;
  let closing = false;

  const server = http.createServer(async (request, response) => {
    const nonce = randomBytes(18).toString("base64url");
    try {
      if (!isAllowedHost(request.headers.host, port)) {
        sendJson(response, 421, { error: "Host 无效" });
        return;
      }
      const origin = `http://127.0.0.1:${port}`;
      const requestUrl = new URL(request.url, origin);
      if (request.method === "GET" && requestUrl.pathname === "/" && requestUrl.searchParams.has("token")) {
        if (!secureEqual(requestUrl.searchParams.get("token"), startupToken)) {
          sendJson(response, 403, { error: "启动令牌无效" });
          return;
        }
        response.writeHead(303, {
          "Cache-Control": "no-store",
          "Location": "/",
          "Referrer-Policy": "no-referrer",
          "Set-Cookie": `codex_bridge=${sessionToken}; HttpOnly; SameSite=Strict; Path=/`,
        });
        response.end();
        return;
      }
      const cookie = parseCookies(request.headers.cookie).get("codex_bridge");
      if (!secureEqual(cookie, sessionToken)) {
        sendJson(response, 403, { error: "本地会话无效" });
        return;
      }
      if (request.method === "GET" && requestUrl.pathname === "/") {
        const body = Buffer.from(renderUi(nonce, csrfToken));
        response.writeHead(200, {
          ...securityHeaders(nonce),
          "Content-Type": "text/html; charset=utf-8",
          "Content-Length": body.length,
        });
        response.end(body);
        return;
      }
      if (request.method === "GET" && requestUrl.pathname === "/favicon.ico") {
        response.writeHead(204, { "Cache-Control": "no-store" });
        response.end();
        return;
      }
      if (request.method === "GET" && requestUrl.pathname === "/api/status") {
        sendJson(response, 200, {
          probe: await probe(),
          configured: Boolean(runtimeConfig.baseUrl && runtimeConfig.apiKey),
          hasApiKey: Boolean(runtimeConfig.apiKey),
          baseUrl: runtimeConfig.baseUrl || null,
          imageEnabled: runtimeConfig.imageEnabled,
        });
        return;
      }
      if (request.method !== "POST" || !requestUrl.pathname.startsWith("/api/")) {
        sendJson(response, 404, { error: "未找到" });
        return;
      }
      if (request.headers.origin !== origin) {
        sendJson(response, 403, { error: "Origin 无效" });
        return;
      }
      if (!secureEqual(request.headers["x-csrf-token"], csrfToken)) {
        sendJson(response, 403, { error: "CSRF 令牌无效" });
        return;
      }
      const body = await readJsonRequest(request);
      if (requestUrl.pathname === "/api/save") {
        const baseUrl = normalizeBaseUrl(body.baseUrl);
        const suppliedKey = typeof body.apiKey === "string" ? body.apiKey.trim() : "";
        const apiKey = suppliedKey || runtimeConfig.apiKey;
        if (!apiKey) throw fail("请先输入 API Key", "API_KEY_MISSING");
        if (apiKey.length > 8192) throw fail("API Key 长度无效", "API_KEY_MISSING");
        runtimeConfig = { baseUrl, apiKey, imageEnabled: body.imageEnabled !== false };
        sendJson(response, 200, { saved: true, baseUrl, hasApiKey: true, imageEnabled: runtimeConfig.imageEnabled });
      } else if (requestUrl.pathname === "/api/test") {
        if (!runtimeConfig.baseUrl || !runtimeConfig.apiKey) throw fail("请先保存设置", "API_KEY_MISSING");
        sendJson(response, 200, await testConnection(runtimeConfig));
      } else if (requestUrl.pathname === "/api/launch") {
        if (!runtimeConfig.baseUrl || !runtimeConfig.apiKey) throw fail("请先保存设置", "API_KEY_MISSING");
        sendJson(response, 200, await launch(runtimeConfig));
        if (shutdownAfterLaunch && !closing) {
          closing = true;
          setTimeout(() => {
            server.close();
            server.closeAllConnections?.();
          }, 1200);
        }
      } else {
        sendJson(response, 404, { error: "未找到" });
      }
    } catch (error) {
      const status = error?.code === "UNSUPPORTED_CONTENT_TYPE" ? 415 : ["INVALID_JSON", "HTTP_BODY_TOO_LARGE"].includes(error?.code) ? 400 : 422;
      sendJson(response, status, { error: publicError(error) });
    }
  });

  server.on("clientError", (_error, socket) => socket.end("HTTP/1.1 400 Bad Request\r\n\r\n"));
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  port = server.address().port;
  const url = `http://127.0.0.1:${port}`;
  const bootstrapUrl = `${url}/?token=${encodeURIComponent(startupToken)}`;
  if (openBrowser) openDefaultBrowser(bootstrapUrl);
  return {
    server,
    port,
    url,
    bootstrapUrl,
    close: () =>
      new Promise((resolve) => {
        server.close(resolve);
        server.closeAllConnections?.();
      }),
  };
}

function printHelp() {
  process.stdout.write(`Comidea Codex Image Bridge for macOS\n\nUsage:\n  node codex-image-bridge.mjs\n  node codex-image-bridge.mjs --probe --json\n\nThe default command opens the local setup UI.\n`);
}

export async function main(args = process.argv.slice(2)) {
  if (process.env.CODEX_IMAGE_BRIDGE_ENABLED === "1") return runCliProxy(args);
  if (args[0] === "--probe") {
    const report = await probeSystem();
    process.stdout.write(`${JSON.stringify(report, null, args.includes("--json") ? 2 : 0)}\n`);
    return 0;
  }
  if (args.length === 1 && ["-h", "--help"].includes(args[0])) {
    printHelp();
    return 0;
  }
  if (args.length > 0) return runCliProxy(args);
  requireNode24();
  const ui = await startUiServer();
  process.stdout.write(`Comidea Codex Image Bridge: ${ui.url}\n`);
  return new Promise((resolve) => ui.server.once("close", () => resolve(0)));
}

const executedPath = process.argv[1] ? path.resolve(process.argv[1]) : "";
if (executedPath === path.resolve(MODULE_PATH)) {
  main()
    .then((exitCode) => {
      process.exitCode = exitCode;
    })
    .catch((error) => {
      process.stderr.write(`codex-image-bridge: ${publicError(error)}\n`);
      process.exitCode = 1;
    });
}

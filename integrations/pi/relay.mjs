// Managed by pi-qq-herdr-client; protocol 1.
import fs from 'node:fs/promises';
import { createHash, createHmac, randomUUID, timingSafeEqual } from 'node:crypto';
import { isAbsolute, resolve, join } from 'node:path';
import { homedir, tmpdir } from 'node:os';

const cache = process.env.XDG_CACHE_HOME;
const root = join(cache && isAbsolute(cache) ? cache : join(homedir(), '.cache'), 'pi-qq-herdr-client');
const marker = '// Managed by pi-qq-herdr-client; protocol 1.';
const companions = ['pi-qq-client.ts', 'pi-qq-client/connection.ts',
  'pi-qq-client/commands.ts', 'pi-qq-client/permissions.ts', 'pi-qq-client/relay.mjs'];
const relayId = /^[a-f0-9]{32}$/;
const requestId = /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/;
const missing = (error) => error?.code === 'ENOENT';

async function atomic(path, bytes) {
  const temp = `${path}.${randomUUID()}.tmp`;
  try {
    await fs.writeFile(temp, bytes, { mode: 0o600, flag: 'wx' });
    await fs.rename(temp, path);
  } finally { await fs.rm(temp, { force: true }); }
}

export function relayDirectory(id) {
  if (!relayId.test(id)) throw new Error('Invalid Pi QQ relay');
  return join(root, 'sessions', id);
}

function mac(key, direction, body) {
  return createHmac('sha256', Buffer.from(key, 'base64')).update(`${direction}.${body}`).digest('hex');
}

function signed(key, direction, value) {
  const body = Buffer.from(JSON.stringify(value)).toString('base64');
  return { body, mac: mac(key, direction, body) };
}

function verify(key, direction, { body, mac: tag } = {}) {
  if (typeof body !== 'string' || !/^[a-f0-9]{64}$/.test(tag || '') ||
    !timingSafeEqual(Buffer.from(tag, 'hex'), Buffer.from(mac(key, direction, body), 'hex'))) {
    throw new Error('Pi QQ relay authentication failed');
  }
  return JSON.parse(Buffer.from(body, 'base64').toString('utf8'));
}

// Called on the target by Herdr machine.exec, with all data on stdin.
export async function provision({ files, relay, key }) {
  if (!relayId.test(relay) || Buffer.from(key || '', 'base64').length !== 32 ||
    !companions.every((name) => files[name]?.startsWith(marker))) throw new Error('Invalid Pi QQ companion bundle');
  const hash = createHash('sha256').update(JSON.stringify(files)).digest('hex').slice(0, 32);
  const bundle = join(root, 'bundles', hash);
  if (process.platform === 'win32') throw new Error('Pi QQ remote relay requires macOS or Linux');
  const agent = resolve(process.env.PI_CODING_AGENT_DIR || join(homedir(), '.pi', 'agent'));
  const extensions = join(agent, 'extensions');
  const installed = new Map();
  for (const name of companions) {
    const path = join(extensions, name);
    try {
      if (!(await fs.lstat(path)).isFile()) return { error: 'extension_conflict' };
      const current = await fs.readFile(path, 'utf8');
      if (!current.startsWith(marker)) return { error: 'extension_conflict' };
      installed.set(name, current);
    } catch (error) {
      if (!missing(error)) throw error;
    }
  }
  await fs.mkdir(join(extensions, 'pi-qq-client'), { recursive: true, mode: 0o700 });
  await fs.mkdir(bundle, { recursive: true, mode: 0o700 });
  await atomic(join(bundle, 'relay.mjs'), files['pi-qq-client/relay.mjs']);
  for (const name of [...companions.slice(1), companions[0]]) {
    if (installed.get(name) !== files[name]) await atomic(join(extensions, name), files[name]);
  }
  const dir = relayDirectory(relay);
  await fs.mkdir(join(dir, 'in'), { recursive: true, mode: 0o700 });
  await fs.mkdir(join(dir, 'out'), { recursive: true, mode: 0o700 });
  await fs.mkdir(join(dir, 'files'), { recursive: true, mode: 0o700 });
  await atomic(join(dir, 'key'), key);
  return { extension: join(extensions, 'pi-qq-client.ts'), bundle: hash, home: homedir() };
}

async function freshPeer(dir) {
  const peer = await readJson(join(dir, 'peer.json'));
  if (peer && relayId.test(peer.instance) && Date.now() - peer.written <= 6500) return peer;
}

export async function exchange(id, packet) {
  const dir = relayDirectory(id);
  const key = await fs.readFile(join(dir, 'key'), 'utf8');
  const request = verify(key, 'desktop', packet);
  const started = Date.now();
  const { instance, generation, messages, round } = request;
  if ((instance && (!relayId.test(instance) || !requestId.test(generation))) ||
      !Number.isSafeInteger(round) || round < 1 || messages.length > 16) throw new Error('Invalid Pi QQ connection');
  let peer = await freshPeer(dir);
  const sameConnection = (peer) => peer?.instance === instance && peer?.generation === generation;
  const registering = messages.length === 1 && messages[0].registration === true && messages[0].generation === generation;
  if (sameConnection(peer) || (registering && peer?.instance === instance)) {
    const heartbeat = { generation, ack: request.ack, expires: started + 6500 };
    await atomic(join(dir, 'in', `${instance}-heartbeat.json`), JSON.stringify(heartbeat));
    if (messages.length) {
      if (!messages.every((message) => requestId.test(message.requestId))) throw new Error('Invalid Pi QQ request');
      const batch = `${instance}-${String(round).padStart(16, '0')}.json`;
      await atomic(join(dir, 'in', batch), JSON.stringify({ ...heartbeat, messages }));
    }
  }
  while (peer) {
    const events = [];
    const same = sameConnection(peer);
    if (same) {
      const folder = join(dir, 'out');
      const prefix = `${peer.instance}-`;
      const sequences = (await fs.readdir(folder)).filter((name) => name.startsWith(prefix) && name.endsWith('.json'))
        .map((name) => Number(name.slice(prefix.length, -5))).filter(Number.isSafeInteger).sort((a, b) => a - b);
      for (const seq of sequences) {
        const path = join(folder, `${prefix}${seq}.json`);
        if (seq <= request.ack) await fs.rm(path, { force: true });
        else if (events.length < 16) {
          const message = await readJson(path);
          if (message) events.push({ seq, message });
        }
      }
    }
    if (events.length || !same || messages.length || Date.now() - started >= 2200) {
      return signed(key, 'pi', { nonce: request.nonce, instance: peer.instance,
        generation: peer.generation, meta: peer.meta, events });
    }
    await new Promise((done) => setTimeout(done, 160));
    peer = await freshPeer(dir);
  }
  return signed(key, 'pi', { nonce: request.nonce });
}

async function readJson(path) {
  try { return JSON.parse(await fs.readFile(path, 'utf8')); }
  catch (error) { if (!missing(error)) throw error; }
}

export async function relayFile(id, packet) {
  const dir = relayDirectory(id);
  const key = await fs.readFile(join(dir, 'key'), 'utf8');
  const request = verify(key, 'file', packet);
  const peer = await freshPeer(dir);
  if (!peer?.generation || peer.instance !== request.instance || peer.generation !== request.generation) {
    throw new Error('Pi QQ connection changed');
  }
  if (!relayId.test(request.id) || !Number.isSafeInteger(request.offset) || request.offset < 0) throw new Error('Invalid Pi QQ attachment');
  const staged = join(dir, 'files', request.id);
  if (request.op === 'put') {
    const bytes = Buffer.from(request.data, 'base64');
    if (bytes.length > 1024 * 1024 || request.offset + bytes.length > 100 * 1024 * 1024) throw new Error('Attachment exceeds limit');
    const file = await fs.open(staged, request.offset ? 'r+' : 'w', 0o600);
    try {
      if ((await file.stat()).size !== request.offset) throw new Error('Attachment offset changed');
      const { bytesWritten } = await file.write(bytes, 0, bytes.length, request.offset);
      if (bytesWritten !== bytes.length) throw new Error('Attachment write incomplete');
    } finally { await file.close(); }
    return signed(key, 'pi', { nonce: request.nonce });
  }
  if (request.op === 'finish') {
    const name = request.name;
    if (!name || name === '.' || name === '..' || /[/\\\x00-\x1f\x7f]/.test(name) ||
      (await fs.stat(staged)).size !== request.offset) throw new Error('Invalid attachment');
    const folder = await fs.mkdtemp(join(tmpdir(), 'pi-qq-'));
    const destination = join(folder, name);
    try { await fs.copyFile(staged, destination); }
    finally { await fs.rm(staged, { force: true }); }
    return signed(key, 'pi', { nonce: request.nonce, path: destination });
  }
  if (request.op !== 'get' || typeof request.path !== 'string' || !request.path.startsWith('/')) throw new Error('Invalid attachment path');
  const file = await fs.open(request.path, 'r');
  try {
    const stat = await file.stat();
    const limit = (request.kind === 'image' ? 30 : 100) * 1024 * 1024;
    if (!stat.isFile() || stat.size > limit || request.offset > stat.size) throw new Error('Attachment exceeds limit');
    const bytes = Buffer.alloc(Math.min(stat.size - request.offset, 1024 * 1024));
    const { bytesRead } = await file.read(bytes, 0, bytes.length, request.offset);
    return signed(key, 'pi', { nonce: request.nonce, size: stat.size, data: bytes.subarray(0, bytesRead).toString('base64') });
  } finally { await file.close(); }
}

// The extension owns one instance per connection, including after Pi reloads.
export class RelaySocket extends EventTarget {
  readyState = 0;
  constructor(id, meta) {
    super();
    this.dir = relayDirectory(id);
    this.meta = meta;
    this.instance = randomUUID().replaceAll('-', '');
    this.generation = '';
    this.batch = 0;
    this.next = 0;
    this.ack = 0;
    this.waiting = new Map();
    this.lastPing = 0;
    this.queued = 0;
    this.writing = Promise.resolve();
    void this.start();
  }
  async start() {
    try {
      await fs.access(join(this.dir, 'key'));
      const owner = join(this.dir, 'owner');
      try {
        await fs.writeFile(owner, String(process.pid), { mode: 0o600, flag: 'wx' });
      } catch (error) {
        if (error.code !== 'EEXIST' || await fs.readFile(owner, 'utf8') !== String(process.pid)) throw error;
      }
      if (this.readyState !== 0) return;
      this.readyState = 1;
      this.dispatchEvent(new Event('open'));
      void this.tick();
      this.timer = setInterval(() => void this.tick(), 220);
      this.timer.unref?.();
    } catch { this.close(); }
  }
  emit(message) {
    this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(message) }));
  }
  send(raw) {
    const message = JSON.parse(raw);
    if (message.type === 'register') { this.registration = message.requestId; return; }
    if (message.type === 'update') return;
    if (!this.generation || Date.now() - this.lastPing > 7500) throw new Error('Desktop client disconnected');
    if (this.queued >= 64 || Buffer.byteLength(raw) > 65_536) throw new Error('Pi QQ relay message limit reached');
    this.queued += 1;
    const seq = ++this.next;
    const publish = async () => {
      if (this.readyState === 1) await atomic(join(this.dir, 'out', `${this.instance}-${seq}.json`), raw);
    };
    this.writing = this.writing.then(publish).catch(() => this.close()).finally(() => { this.queued -= 1; });
    return seq;
  }
  async sendAndWait(raw) {
    if (this.readyState !== 1) throw new Error('Desktop client disconnected');
    const seq = this.send(raw);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiting.delete(seq);
        reject(new Error('Desktop client did not acknowledge the action result'));
      }, 8000);
      timer.unref?.();
      this.waiting.set(seq, { resolve, reject, timer });
    });
  }
  async publish() {
    this.written = Date.now();
    await atomic(join(this.dir, 'peer.json'), JSON.stringify({ instance: this.instance,
      generation: this.generation, meta: this.meta, written: this.written }));
  }
  async tick() {
    if (this.busy || this.readyState !== 1) return;
    this.busy = true;
    try {
      const dir = join(this.dir, 'in');
      const prefix = `${this.instance}-`;
      const heartbeat = await readJson(join(dir, `${prefix}heartbeat.json`));
      const names = (await fs.readdir(dir)).filter((name) => name.startsWith(prefix) && /^\d{16}\.json$/.test(name.slice(prefix.length))).sort();
      for (const name of names) {
        const path = join(dir, name);
        const item = await readJson(path);
        await fs.rm(path, { force: true });
        if (!item || this.readyState !== 1 || Date.now() > item.expires) continue;
        const batch = Number(name.slice(prefix.length, -5));
        const registration = item.messages.length === 1 && item.messages[0].registration === true ? item.messages[0] : undefined;
        if (registration && this.generation && item.generation !== this.generation) { this.close(); return; }
        if (registration && !this.generation && this.registration) {
          this.generation = item.generation;
          this.batch = batch;
          this.lastPing = Date.now();
          this.emit({ ...registration, requestId: this.registration });
          await this.publish();
        } else if (item.generation === this.generation && this.generation && batch > this.batch) {
          this.batch = batch;
          this.lastPing = Date.now();
          for (const message of item.messages) this.emit(message);
        }
      }
      const now = Date.now();
      if (this.generation && heartbeat?.generation === this.generation && heartbeat.expires >= now) {
        this.lastPing = Math.max(this.lastPing, heartbeat.expires - 6500);
        this.ack = Math.max(this.ack, heartbeat.ack);
        for (const [seq, pending] of this.waiting) {
          if (seq > this.ack) continue;
          this.waiting.delete(seq);
          clearTimeout(pending.timer);
          pending.resolve();
        }
      }
      if (this.generation && now - this.lastPing > 8500) { this.close(); return; }
      if (!this.written || now - this.written > 900) await this.publish();
    } catch { this.close(); }
    finally { this.busy = false; }
  }
  close() {
    if (this.readyState === 3) return;
    this.readyState = 3;
    clearInterval(this.timer);
    for (const pending of this.waiting.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error('Desktop client disconnected'));
    }
    this.waiting.clear();
    this.dispatchEvent(new Event('close'));
    const remove = async (dir) => {
      for (const name of await fs.readdir(dir)) {
        if (name.startsWith(`${this.instance}-`)) await fs.rm(join(dir, name), { force: true });
      }
    };
    void this.writing.then(() => Promise.all([remove(join(this.dir, 'in')), remove(join(this.dir, 'out'))])).catch(() => {});
  }
}

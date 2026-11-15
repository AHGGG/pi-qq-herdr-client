// Managed by pi-qq-herdr-client; protocol 1.
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { homedir } from 'node:os';
import { randomUUID } from 'node:crypto';

const root = process.env.PI_QQ_CLIENT_DIR || (process.platform === 'win32'
  ? join(process.env.LOCALAPPDATA || join(homedir(), 'AppData/Local'), 'pi-qq-herdr-client')
  : process.platform === 'darwin' ? join(homedir(), 'Library/Application Support/pi-qq-herdr-client')
    : join(process.env.XDG_DATA_HOME || join(homedir(), '.local/share'), 'pi-qq-herdr-client'));

export class SessionConnection {
  constructor(meta, { onAction, onState }) {
    this.meta = meta; this.onAction = onAction; this.onState = onState;
    this.pending = new Map(); this.closed = false; this.socket = null; this.slot = null;
    this.retry = null; this.watchdog = null;
  }
  async start() { void this.connect(); }
  async connect() {
    if (this.closed) return;
    try {
      const info = JSON.parse(await readFile(join(root, 'bridge.json'), 'utf8'));
      if (info.protocol !== 1 || !Number.isInteger(info.port) || info.port < 1 || info.port > 65535 || !/^[a-f0-9]{64}$/.test(info.token)) throw new Error('Unavailable');
      if (this.closed) return;
      const socket = new WebSocket(`ws://127.0.0.1:${info.port}/session`);
      this.socket = socket;
      this.watchdog = setTimeout(() => socket.close(), 8000); this.watchdog.unref?.();
      socket.addEventListener('error', () => {});
      socket.addEventListener('open', () => {
        void this.call({ type: 'register', protocol: 1, token: info.token, ...this.meta }).then((result) => {
          if (this.socket !== socket || this.closed) return;
          clearTimeout(this.watchdog); this.slot = result.slot; this.onState('connected', result);
        }, () => socket.close());
      });
      socket.addEventListener('message', (event) => {
        if (this.socket !== socket || this.closed) return;
        let message; try { message = JSON.parse(String(event.data)); } catch { socket.close(); return; }
        if (message.type === 'result') {
          const pending = this.pending.get(message.requestId);
          if (!pending) return;
          this.pending.delete(message.requestId); clearTimeout(pending.timer);
          if (message.ok) pending.resolve(message); else pending.reject(new Error(message.error || 'Request failed'));
        } else if (message.type === 'action') {
          void Promise.resolve().then(() => this.onAction(message)).then((result) => {
            const { afterReply, onReplyFailure, ...reply } = result || {};
            if (this.send({ type: 'action_result', requestId: message.requestId, ok: true, ...reply })) afterReply?.();
            else onReplyFailure?.();
          }, (error) => this.send({ type: 'action_result', requestId: message.requestId, ok: false, reason: String(error.message).slice(0, 300) }));
        }
      });
      socket.addEventListener('close', () => {
        if (this.socket !== socket) return;
        clearTimeout(this.watchdog); this.socket = null; this.slot = null;
        for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error('Connection closed; submission is unconfirmed.')); }
        this.pending.clear();
        if (!this.closed) { this.onState('disconnected'); this.schedule(); }
      });
    } catch { if (!this.closed) this.onState('disconnected'); this.schedule(); }
  }
  schedule() {
    if (this.closed) return;
    clearTimeout(this.retry); this.retry = setTimeout(() => void this.connect(), 2000); this.retry.unref?.();
  }
  send(message) {
    if (this.closed || this.socket?.readyState !== WebSocket.OPEN) return false;
    this.socket.send(JSON.stringify(message)); return true;
  }
  update(meta) { Object.assign(this.meta, meta); this.send({ type: 'update', ...meta }); }
  call(message, { timeout = 8000 } = {}) {
    const requestId = randomUUID();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(requestId); reject(new Error('No acknowledgment. Check status before retrying.')); }, timeout);
      timer.unref?.(); this.pending.set(requestId, { resolve, reject, timer });
      if (!this.send({ ...message, requestId })) { clearTimeout(timer); this.pending.delete(requestId); reject(new Error('Desktop client disconnected.')); }
    });
  }
  close() {
    this.closed = true; clearTimeout(this.retry); clearTimeout(this.watchdog);
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error('Session closed')); }
    this.pending.clear(); this.socket?.close(); this.socket = null;
  }
}

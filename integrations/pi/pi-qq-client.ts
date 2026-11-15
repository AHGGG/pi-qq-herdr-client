// Managed by pi-qq-herdr-client; protocol 1.
import { basename, resolve } from 'node:path';
import { homedir } from 'node:os';
import { SessionConnection } from './pi-qq-client/connection.ts';
import { createRemoteCommands } from './pi-qq-client/commands.ts';
import { createRemotePermissions } from './pi-qq-client/permissions.ts';
const cleanLabel = (value, max = 140) => String(value).replace(/[\x00-\x1f\x7f]/g, ' ').slice(0, max);

export default function qqRemote(pi) {
  const Connection = SessionConnection;
  const brokerRequest = async () => {
    if (!connection) throw new Error('Start the Pi QQ desktop client first.');
    return connection.call({ type: 'status' });
  };
  let ctx;
  let connection;
  let generation = 0;
  let compacting = false;
  let compactTimer;
  let stateTimer;
  let terminalPrompts = 0;
  let deferred = [];
  let waitingInputs = [];
  let questions = new Map();
  let answers = new Map();
  let completedText = '';
  let completedDelivery;
  let hadWork = false;
  let muted = false;
  let disconnectedWarningShown = false;
  let forwardingGeneration = 0;
  let customName = '';
  let subscriptions = [];
  let reloadToken;
  const remoteCommands = createRemoteCommands(pi, {
    getContext: () => ctx,
    isBusy: () => compacting || terminalPrompts > 0 || questions.size > 0,
    sync: () => { syncState(); syncRuntime(true); },
    notify: (text) => fireNotice(text),
  });

  const remotePermissions = createRemotePermissions(pi, { getConnection: () => connection, sync: () => syncState(),
    onFeedbackBlocked: () => fireNotice('Your message was submitted, but a permission request is still waiting. Use /permissions or the Pi terminal to resolve it; do not resend the message.'),
  });

  const report = (message, level = 'info') => {
    if (ctx?.hasUI) ctx.ui.notify(message, level);
  };
  const update = (state) => {
    if (connection && connection.meta.state !== state) connection.update({ state });
  };
  const runtime = (refreshUsage = true) => {
    const model = ctx.model;
    const previous = connection?.meta.runtime;
    const sameModel = previous?.model?.provider === model?.provider && previous?.model?.id === model?.id;
    // Context estimation scans history; reuse it during the lightweight state poll.
    const usage = !refreshUsage && sameModel ? previous?.contextUsage : ctx.getContextUsage?.();
    return {
      model: model ? { provider: model.provider, id: model.id } : null,
      thinkingLevel: ctx.thinkingLevel ?? pi.getThinkingLevel?.() ?? null,
      contextUsage: usage ? { tokens: usage.tokens, contextWindow: usage.contextWindow } : null,
      autoForward: !muted,
    };
  };
  const syncRuntime = (refreshUsage = false) => {
    if (!connection || !ctx) return;
    const current = runtime(refreshUsage);
    if (JSON.stringify(connection.meta.runtime) !== JSON.stringify(current)) connection.update({ runtime: current });
  };
  // Lifecycle events update immediately; reconcile against Pi when an event was missed.
  const syncState = () => {
    if (!connection || !ctx) return;
    const idle = ctx.isIdle();
    update(remotePermissions.size ? 'waiting_for_permission' : !idle && questions.size ? 'waiting_for_answer'
      : terminalPrompts ? 'waiting_for_input'
        : idle ? 'idle'
          : compacting ? 'compacting' : 'working');
    syncRuntime();
  };
  const label = () => ctx?.sessionManager.getSessionName() || customName ||
    `${basename(ctx?.cwd || 'Pi')} ${ctx?.sessionManager.getSessionId().slice(0, 6)}`;
  const saveSettings = () => pi.appendEntry('pi-qq-client-settings', { muted, name: customName });
  const updateSessionName = () => {
    const name = pi.getSessionName() || '';
    if (customName !== name) { customName = name; saveSettings(); }
    connection?.update({ label: label(), name });
  };
  const applySessionName = (name) => {
    if (pi.getSessionName() !== name) pi.setSessionName(name);
    updateSessionName();
  };

  async function notify(text, { imagePath, videoPath, filePath } = {}) {
    if (!connection) throw new Error('QQ_SESSION_DISABLED: run /qq on');
    return connection.call({ type: 'notify', text, ...(imagePath ? { imagePath } : {}), ...(videoPath ? { videoPath } : {}), ...(filePath ? { filePath } : {}) },
      { timeout: imagePath || videoPath || filePath ? 30_000 : 6000 });
  }

  function fireNotice(text) {
    return notify(text).then(() => true, (error) => {
      pi.appendEntry('pi-qq-client-unsent', { text, error: error.message, timestamp: Date.now() });
      if (connection?.slot) {
        report(`QQ 通知未入队：${error.message}。请查看 Pi QQ 客户端的连接状态；此条消息仍可在终端查看。`, 'warning');
      } else if (!disconnectedWarningShown) {
        disconnectedWarningShown = true;
        report('QQ 通知未入队：请打开并保持 Pi QQ 桌面客户端运行，扩展会自动重连。此条消息尚未发送，请在终端查看。', 'warning');
      }
      return false;
    });
  }

  function inject(action) {
    const text = `From QQ user:\n${action.text}`;
    return new Promise((resolve, reject) => {
      const pending = { text, resolve, reject, timer: null };
      pending.timer = setTimeout(() => {
        waitingInputs = waitingInputs.filter((item) => item !== pending);
        reject(new Error('Pi did not confirm submission. Check the terminal before resending.'));
      }, 4500);
      waitingInputs.push(pending);
      try { pi.sendUserMessage(text, { deliverAs: action.mode === 'steer' ? 'steer' : 'followUp', expandPromptTemplates: false }); }
      catch (error) {
        clearTimeout(pending.timer);
        waitingInputs = waitingInputs.filter((item) => item !== pending);
        reject(error);
      }
    });
  }

  function finishCompaction() {
    compacting = false;
    clearInterval(compactTimer);
    const pending = deferred;
    deferred = [];
    for (const action of pending) {
      void inject(action).catch(() => fireNotice('A QQ message was not confirmed after compaction. Check the terminal before resending.'));
    }
    syncState();
    syncRuntime(true);
  }

  async function handleAction(action) {
    if (action.action === 'permission_list') return { status: 'completed', requests: remotePermissions.list() };
    if (action.action === 'permission_answer') return remotePermissions.answer(action);
    if (action.action === 'command') return remoteCommands.execute(action);
    if (remoteCommands.reloading) throw new Error('This Pi is reloading. Wait for it to reconnect before sending another action.');
    if (action.action === 'stop') {
      deferred = [];
      ctx.abort();
      syncState();
      return { status: 'stopping' };
    }
    if (action.action === 'prompt') {
      if (typeof action.text !== 'string' || !action.text.trim()) throw new Error('The message is empty.');
      if (compacting) {
        if (deferred.length >= 20) throw new Error('The compaction queue is full. Try again later.');
        deferred.push(action);
        return { status: 'compacting' };
      }
      // Submit the original text first. A failed submission must leave the
      // human decision alone. Releasing the wait then lets Pi drain its queue.
      const result = await inject(action);
      try {
        return await remotePermissions.interruptForFeedback() ? { ...result, permissionFeedback: 'cancelled' } : result;
      } catch {
        return { ...result, permissionFeedback: 'waiting' };
      }
    }
    if (action.action === 'name') {
      if (typeof action.name !== 'string' || !action.name.trim() || Array.from(action.name).length > 70) throw new Error('Invalid session name.');
      applySessionName(action.name);
      return { status: 'renamed' };
    }
    if (action.action === 'answer') {
      if (!questions.has(action.toolCallId)) throw new Error('This questionnaire is closed or belongs to another session.');
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          answers.delete(action.requestId);
          reject(new Error('The answer was not confirmed. Check the terminal before resending.'));
        }, 4500);
        answers.set(action.requestId, { resolve, reject, timer });
        pi.events.emit('rpiv:ask-user:answer', {
          requestId: action.requestId,
          toolCallId: action.toolCallId,
          ...(action.cancel ? { cancel: true } : { answers: action.answers }),
        });
      });
    }
    throw new Error('Unknown remote action.');
  }

  async function start(currentCtx) {
    if (connection) return true;
    ctx = currentCtx;
    const ownGeneration = ++generation;
    const sessionId = ctx.sessionManager.getSessionId();
    const current = new Connection({ id: `${sessionId}:${process.pid}`, sessionId, label: label(), cwd: ctx.cwd, pid: process.pid,
      state: ctx.isIdle() ? 'idle' : 'working', terminal: process.env.HC_TERMINAL_ID,
      terminalSocket: process.env.HC_TERMINAL_SOCKET, runtime: runtime(),
      capabilities: ['prompt', 'stop', 'name', 'commands', 'question-answer', 'permission-answer'], reloadToken,
    }, {
      onAction: (action) => {
        if (ownGeneration !== generation) throw new Error('The session has changed.');
        return handleAction(action);
      },
      onState: (state, registration) => {
        if (ownGeneration !== generation) return;
        if (ctx.hasUI) ctx.ui.setStatus('pi-qq-client', state === 'connected' ? `Pi QQ ${current.slot}` : 'Pi QQ 等待桌面客户端');
        if (state === 'connected') {
          disconnectedWarningShown = false;
          if (registration?.name) applySessionName(registration.name);
          syncState();
          for (const payload of questions.values()) current.send({ type: 'question_open', ...payload });
          remotePermissions.reannounce();
        }
      },
    });
    connection = current;
    try {
      await current.start();
      if (connection !== current) return false;
      syncState();
      stateTimer = setInterval(syncState, 2000);
      stateTimer.unref?.();
    }
    catch {
      if (connection === current) { clearInterval(stateTimer); current.close(); connection = undefined; }
      // A stopped desktop client is normal; the next connection retries quietly.
      return false;
    }
    return true;
  }

  function stop() {
    generation += 1;
    connection?.close();
    connection = undefined;
    clearInterval(stateTimer);
    clearInterval(compactTimer);
    terminalPrompts = 0;
    compacting = false;
    completedText = '';
    completedDelivery = undefined;
    hadWork = false;
    deferred = [];
    for (const pending of waitingInputs) { clearTimeout(pending.timer); pending.reject(new Error('The session is closed.')); }
    waitingInputs = [];
    for (const pending of answers.values()) { clearTimeout(pending.timer); pending.reject(new Error('The session is closed.')); }
    answers.clear();
    questions.clear();
    if (ctx?.hasUI) ctx.ui.setStatus('pi-qq-client', undefined);
  }

  pi.on('session_start', async (_event, currentCtx) => {
    ctx = currentCtx;
    remotePermissions.reset();
    muted = false;
    customName = '';
    reloadToken = undefined;
    if (!subscriptions.length) {
      subscriptions = Object.entries(busHandlers).map(([channel, handler]) => pi.events.on(channel, handler));
    }
    for (const entry of ctx.sessionManager.getBranch()) {
      if (entry.type === 'custom' && entry.customType === 'pi-qq-client-settings') {
        muted = entry.data?.muted === true;
        customName = cleanLabel(entry.data?.name || '', 140);
      }
      if (entry.type === 'custom' && entry.customType === 'pi-qq-client-reload' &&
          entry.data?.pid === process.pid && Date.now() - entry.data.at >= 0 && Date.now() - entry.data.at < 120000 &&
          /^[a-f0-9-]{36}$/.test(entry.data.token || '')) reloadToken = entry.data.token;
    }
    // Interactive main sessions register automatically; print-mode agents stay out.
    if (ctx.mode === 'tui' || (ctx.mode === 'rpc' && process.env.PI_QQ_CLIENT_INCLUDE_RPC === '1')) {
      void start(ctx);
    }
  });
  pi.on('session_shutdown', async () => {
    remotePermissions.reset();
    remoteCommands.stop();
    if (deferred.length && connection?.slot) {
      await notify(`Session closed with ${deferred.length} message(s) waiting for compaction. Select a session before resending.`).catch(() => {});
    }
    stop();
    for (const unsubscribe of subscriptions) unsubscribe();
    subscriptions = [];
  });
  pi.on('session_info_changed', updateSessionName);
  pi.on('model_select', (_event, currentCtx) => { ctx = currentCtx; syncState(); });
  pi.on('thinking_level_select', (_event, currentCtx) => { ctx = currentCtx; syncState(); });
  pi.on('before_agent_start', (event) => {
    const status = !connection
      ? 'Automatic QQ text forwarding is off for this session. qq_send is unavailable until /qq on enables the connection.'
      : muted
        ? 'Automatic QQ text forwarding is muted by /qq mute. Do not use qq_send merely to bypass that preference; explicitly requested messages remain available.'
        : `Automatic QQ text forwarding is enabled${connection.slot ? '' : ', but the bridge is currently reconnecting and cannot confirm enqueueing'}.`;
    event.systemPromptOptions.sections.qq_remote = [
      status,
      'When enabled, the extension queues the visible text from each completed assistant message (message_end), including intermediate updates and the final reply. Write these messages normally; do not duplicate their text with qq_send or the CLI. Text over 5500 characters is shortened with a terminal hint.',
      'A separate completion notice is queued once Pi fully settles (agent_settled), after retries, compaction retries and queued continuations finish. It does not repeat a reply already confirmed in the durable queue; a final reply not confirmed in the queue is included in the completion notice.',
      'Thinking and tool output are not automatically forwarded. Use qq_send for a separately requested message or attachment. Its imagePath, videoPath or filePath uploads a local file; automatic forwarding sends text only. Supported questionnaires and terminal confirmation notices are forwarded separately while the connection is enabled.',
      'Incoming QQ attachments are saved unchanged under the system temporary directory and arrive as absolute file paths. Generic files retain a safe original filename. The Harness and Agent choose how to process them; the bridge does not parse, unpack or execute files, extract frames, transcode video or transcribe audio.',
      'Automatic forwarding requires a working connection at completion. A queue receipt is not proof of a phone notification; do not claim delivery without evidence.',
    ].join('\n');
  });
  pi.on('input', (event) => {
    if (event.source !== 'extension') return;
    const index = waitingInputs.findIndex((pending) => pending.text === event.text);
    if (index === -1) return;
    const [pending] = waitingInputs.splice(index, 1);
    clearTimeout(pending.timer);
    pending.resolve({ status: 'submitted' });
  });
  pi.on('session_before_compact', () => {
    compacting = true;
    update('compacting');
    clearInterval(compactTimer);
    compactTimer = setInterval(() => { if (ctx?.isIdle()) finishCompaction(); }, 1000);
    compactTimer.unref?.();
  });
  pi.on('session_compact', finishCompaction);
  pi.on('session_compact_failed', finishCompaction);
  pi.on('turn_end', () => remotePermissions.finishTurn());
  pi.on('agent_start', () => { hadWork = true; completedText = ''; completedDelivery = undefined; update('working'); });
  pi.on('message_end', (event) => {
    const message = event.message;
    if (message.role !== 'assistant') return;
    syncRuntime(true);
    if (muted || !connection || message.stopReason === 'aborted') {
      completedText = '';
      completedDelivery = undefined;
      return;
    }
    if (message.stopReason === 'error') {
      completedText = 'Model call failed. Check the terminal for details.';
      completedDelivery = undefined;
      return;
    }
    const text = (message.content || []).filter((part) => part.type === 'text').map((part) => part.text).join('\n').trim();
    if (!text) return;
    completedText = text.length > 5500 ? `${text.slice(0, 5500)}\n(Output shortened. See the terminal for the full result.)` : text;
    // Queue each complete visible message, even when it also contains tool calls.
    completedDelivery = fireNotice(completedText);
  });
  pi.on('agent_settled', (event) => {
    remotePermissions.finishTurn();
    update('idle');
    syncRuntime(true);
    if (!hadWork) return;
    hadWork = false;
    const text = completedText;
    const delivery = completedDelivery;
    const settledConnection = connection;
    const settledGeneration = generation;
    const settledForwardingGeneration = forwardingGeneration;
    completedText = '';
    completedDelivery = undefined;
    if (muted || !settledConnection) return;
    if (event.aborted) { fireNotice('Task stopped.'); return; }
    // Observe enqueue confirmation without holding Pi's next input behind network I/O.
    void Promise.resolve(delivery).then((queued) => {
      if (muted || connection !== settledConnection || generation !== settledGeneration ||
          forwardingGeneration !== settledForwardingGeneration) return;
      fireNotice(queued ? 'Task finished.' : `Task finished\n${text || 'See the terminal for the full output.'}`);
    });
  });
  pi.on('ui_prompt_start', (event) => {
    terminalPrompts += 1;
    syncState();
    if (connection && !questions.size && !remotePermissions.size) fireNotice(`Waiting for terminal confirmation${event.title ? `: ${cleanLabel(event.title, 180)}` : ''}. Handle permission approvals in the terminal.`);
  });
  pi.on('ui_prompt_end', () => {
    terminalPrompts = Math.max(0, terminalPrompts - 1);
    syncState();
  });

  const busHandlers = {
    'permissions:external-prompt': (payload) => remotePermissions.onPrompt(payload),
    'permissions:external-close': (payload) => remotePermissions.onClose(payload),
    'permissions:answer-result': (payload) => remotePermissions.onAnswer(payload),
    'rpiv:ask-user:prompt': (payload) => {
    if (ctx?.mode !== 'tui' || !payload?.toolCallId) return;
    questions.set(payload.toolCallId, payload);
    connection?.send({ type: 'question_open', ...payload });
    },
    'rpiv:ask-user:blocked': (payload) => {
    if (payload.active) update('waiting_for_answer');
    else {
      questions.delete(payload.toolCallId);
      connection?.send({ type: 'question_close', toolCallId: payload.toolCallId });
      syncState();
    }
    },
    'rpiv:ask-user:answer-result': (payload) => {
    const pending = answers.get(payload?.requestId);
    if (!pending) return;
    clearTimeout(pending.timer);
    answers.delete(payload.requestId);
    if (payload.ok) pending.resolve({ status: 'answered' });
    else pending.reject(new Error(payload.code || 'The answer was not accepted.'));
    },
  };

  pi.registerTool({
    name: 'qq_send', label: 'QQ Send',
    description: 'Send authorized text or one local attachment to the paired QQ owner. Supply text and/or one of imagePath, videoPath, filePath; path fields are mutually exclusive. Images: JPG, PNG, GIF, WebP, BMP; up to 30 MiB. Videos: MP4, MOV, WebM, MKV, AVI; up to 100 MiB. filePath sends any file as a downloadable attachment, up to 100 MiB, preserving a safe filename. Paths are relative to the current project or absolute. Enabled automatic forwarding queues each completed assistant message, including progress and final text, then a completion notice once Pi fully settles (agent_settled). Do not duplicate automatically forwarded text with qq_send. Attachments require explicit sending. Returns a durable queue receipt, not proof of a phone notification. Text limit: 19000 characters.',
    promptSnippet: 'Send authorized QQ messages or attachments; enabled automatic forwarding handles completed assistant text and task completion.',
    parameters: { type: 'object', properties: {
      text: { type: 'string', minLength: 1, maxLength: 19000 },
      imagePath: { type: 'string', minLength: 1, maxLength: 4096, description: 'Local image path, absolute or relative to the current project.' },
      videoPath: { type: 'string', minLength: 1, maxLength: 4096, description: 'Local video path, absolute or relative to the current project. Mutually exclusive with imagePath and filePath.' },
      filePath: { type: 'string', minLength: 1, maxLength: 4096, description: 'Local file path, absolute or relative to the current project. Sends a downloadable file, retaining a safe filename. Mutually exclusive with imagePath and videoPath.' },
    }, additionalProperties: false },
    async execute(_id, params, _signal, _onUpdate, currentCtx) {
      if ([params.imagePath, params.videoPath, params.filePath].filter((path) => path !== undefined).length > 1) throw new Error('Supply only one of imagePath, videoPath or filePath.');
      if (!params.text?.trim() && !params.imagePath?.trim() && !params.videoPath?.trim() && !params.filePath?.trim()) throw new Error('Supply text, imagePath, videoPath or filePath.');
      const localPath = (value) => {
        const input = value?.replace(/^@/, '');
        return input ? resolve(currentCtx?.cwd || ctx.cwd, input.startsWith('~/') || input.startsWith('~\\') ? homedir() + input.slice(1) : input) : undefined;
      };
      const receipt = await notify(params.text || '', { imagePath: localPath(params.imagePath), videoPath: localPath(params.videoPath), filePath: localPath(params.filePath) });
      return { content: [{ type: 'text', text: JSON.stringify(receipt) }], details: receipt };
    },
  });
  pi.registerTool({
    name: 'qq_status', label: 'QQ Status', description: 'Inspect the QQ gateway, connected Pi sessions and pending delivery counts without exposing credentials.',
    parameters: { type: 'object', properties: {}, additionalProperties: false },
    async execute() {
      const status = await brokerRequest('/status');
      return { content: [{ type: 'text', text: JSON.stringify(status) }], details: status };
    },
  });
  pi.registerCommand('qq', {
    description: 'QQ remote: on, off, status, send TEXT, name NAME, mute, unmute',
    handler: async (args, currentCtx) => {
      ctx = currentCtx;
      const [command = 'status', ...parts] = args.trim().split(/\s+/).filter(Boolean);
      if (command === 'on') { if (await start(ctx)) report('Connecting this session to the QQ gateway.'); }
      else if (command === 'off') { stop(); report('This session is disconnected from QQ. Other sessions remain connected.'); }
      else if (command === 'name') {
        const name = cleanLabel(parts.join(' '), 140);
        if (!name || Array.from(name).length > 70) { report('Enter a session name with 1 to 70 characters.', 'warning'); return; }
        applySessionName(name);
      }
      else if (command === 'mute' || command === 'unmute') {
        muted = command === 'mute';
        if (muted) { forwardingGeneration += 1; completedText = ''; completedDelivery = undefined; }
        saveSettings(); syncRuntime();
        report(muted ? 'Automatic task messages are muted for this session.' : 'Automatic task messages are enabled for this session.');
      }
      else if (command === 'send') report(JSON.stringify(await notify(parts.join(' '))));
      else if (command === 'status') report(JSON.stringify(await brokerRequest('/status')));
      else report('/qq on | off | status | send TEXT | name NAME | mute | unmute');
    },
  });
}

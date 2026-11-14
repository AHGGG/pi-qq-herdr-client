// Managed by pi-qq-herdr-client; protocol 1.
// Optional permission-system external human-answer transport.
import { randomUUID } from 'node:crypto';
export function permissionSummary(event) {
  const request = event?.request;
  if (event?.protocol !== 1 || typeof event.requestId !== 'string' || !event.requestId || event.requestId.length > 220 ||
      !request || typeof request.surface !== 'string' || typeof request.value !== 'string') return null;
  const evidence = event.evidence ?? [];
  if (!Array.isArray(evidence) || evidence.some((item) => !item || typeof item.label !== 'string' ||
      typeof item.text !== 'string' || (item.detail != null && typeof item.detail !== 'string'))) return null;
  const lines = [
    `Gate: ${request.surface}`,
    ...(request.toolName ? [`Tool: ${request.toolName}`] : []),
    ...(request.invokedToolName ? [`Invoked as: ${request.invokedToolName}`] : []),
    ...(request.requester?.agentName ? [`Agent: ${request.requester.agentName}`] : []),
    ...(request.requester?.forwarded ? [`From subagent: ${request.requester.sessionId || 'Unknown'}`] : []),
    ...(request.matchedPattern ? [`Rule: ${request.matchedPattern}`] : []),
    `Operation:\n${request.value}`,
    ...(request.matchedSpelling ? [`Matched spelling:\n${request.matchedSpelling}`] : []),
    ...(request.executedUnit && request.executedUnit !== request.value ? [`Executed unit:\n${request.executedUnit}`] : []),
    ...(request.commandContext ? [`Command context: ${request.commandContext}`] : []),
    ...(request.decidingPath ? [`Path: ${request.decidingPath.path}\nAccess: ${request.decidingPath.effect}\nAccess source: ${request.decidingPath.effectSource}`] : []),
    ...evidence.map((item) => `${item.label}:\n${item.text}${item.detail ? `\n${item.detail}` : ''}`),
  ];
  const text = lines.join('\n');
  // Never offer approval for a silently shortened operation. The original
  // terminal dialog remains available for requests outside the transport budget.
  const escapedLength = text.replace(/([\\`*_{}\[\]()#+.!<>|~-])/g, '\\$1').length;
  return text.length <= 12000 && escapedLength <= 18000 ? { requestId: event.requestId, text } : null;
}

export function createRemotePermissions(pi, { getConnection, sync, onFeedbackBlocked = () => {} }) {
  const open = new Map();
  const pending = new Map();
  const answering = new Set();
  let feedback = false;
  function sendOpen(prompt) { getConnection()?.send({ type: 'permission_open', ...prompt }); }
  async function decline(permissionId) {
    if (!open.has(permissionId) || answering.has(permissionId)) return;
    try {
      await remote.answer({ permissionId, requestId: randomUUID(), decision: 'deny',
        reason: 'The user sent a new QQ message while permission was pending. Read that message before retrying this operation.' });
    } catch (error) {
      if (open.has(permissionId)) throw error;
    }
  }
  const remote = {
    get size() { return open.size; },
    list() { return [...open.values()]; },
    onPrompt(event) {
      const prompt = permissionSummary(event);
      if (!prompt || open.has(prompt.requestId)) return;
      open.set(prompt.requestId, prompt);
      sync();
      if (feedback) void decline(prompt.requestId).catch(onFeedbackBlocked);
      else sendOpen(prompt);
    },
    async interruptForFeedback() {
      if (!open.size) return false;
      // Later tools in the same batch can open another serialized permission
      // dialog before Pi drains its steering queue. Decline those too, only
      // until turn_end; the model's next plan must ask normally again.
      feedback = true;
      await Promise.all([...open.keys()].map(decline));
      return true;
    },
    finishTurn() { feedback = false; },
    onClose(event) {
      if (!open.delete(event?.requestId)) return;
      getConnection()?.send({ type: 'permission_close', permissionId: event.requestId });
      sync();
    },
    onAnswer(event) {
      const waiting = pending.get(event?.replyId);
      if (!waiting || waiting.permissionId !== event.requestId) return;
      pending.delete(event.replyId);
      clearTimeout(waiting.timer);
      answering.delete(waiting.permissionId);
      if (event.ok === true && event.decision === waiting.decision) waiting.resolve({ status: 'answered', decision: event.decision });
      else waiting.reject(new Error('This permission request is already closed or the answer was rejected.'));
    },
    reannounce() { for (const prompt of open.values()) sendOpen(prompt); },
    answer(action) {
      if (!open.has(action.permissionId)) throw new Error('This permission request is closed or belongs to another session.');
      if (!['allow', 'deny'].includes(action.decision) || typeof action.requestId !== 'string' ||
          (action.reason !== undefined && (typeof action.reason !== 'string' || action.reason.length > 1000))) throw new Error('Invalid permission answer.');
      if (answering.has(action.permissionId)) throw new Error('An answer is already being submitted for this permission request.');
      answering.add(action.permissionId);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(action.requestId);
          answering.delete(action.permissionId);
          reject(new Error('The permission answer was not confirmed. Check its status or the terminal before retrying.'));
        }, 4500);
        pending.set(action.requestId, { permissionId: action.permissionId, decision: action.decision, resolve, reject, timer });
        pi.events.emit('permissions:answer', { requestId: action.permissionId, replyId: action.requestId,
          decision: action.decision, ...(action.reason ? { reason: action.reason } : {}) });
      });
    },
    reset() {
      feedback = false;
      open.clear();
      answering.clear();
      for (const waiting of pending.values()) { clearTimeout(waiting.timer); waiting.reject(new Error('The session is closed.')); }
      pending.clear();
    },
  };
  return remote;
}

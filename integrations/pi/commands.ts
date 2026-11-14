// Managed by pi-qq-herdr-client; protocol 1.
// TypeScript dependencies refresh with Pi /reload.
const cleanLabel = (value, max = 140) => String(value).replace(/[\x00-\x1f\x7f]/g, ' ').slice(0, max);

const reloadCommand = 'pi-qq-client-reload';
const thinkingLevels = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

function pageOf(items, argument, command) {
  if (argument && !/^[1-9]\d*$/.test(argument)) throw new Error(`Use /${command} followed by a page number.`);
  const pages = Math.max(1, Math.ceil(items.length / 3));
  const page = Math.min(Number(argument || 1), pages);
  return { items: items.slice((page - 1) * 3, page * 3), page, pages,
    next: page < pages ? `\nNext: /${command} ${page + 1}` : '' };
}

export function createRemoteCommands(pi, { getContext, isBusy, sync, notify }) {
  const reloads = new Map();
  let stopped = false;
  let changing = false;
  let compacting = false;
  let reloading = false;
  const commands = () => pi.getCommands().filter((command) => command.name !== reloadCommand);
  const requireIdle = (current = getContext()) => {
    if (changing || compacting || reloading || isBusy() || !current.isIdle() || current.hasPendingMessages?.()) {
      throw new Error('Pi is busy or waiting for input. Wait for its current task to finish, then retry this command.');
    }
  };
  const later = (work) => () => {
    if (stopped) return;
    try { work(); }
    catch { notify('The command could not start. Check the Pi terminal before retrying.'); }
  };

  // Only a locally issued one-use ticket can obtain a command-capable reload context.
  pi.registerCommand(reloadCommand, {
    description: 'Internal QQ reload dispatcher',
    handler: async (token, currentCtx) => {
      const pending = reloads.get(token);
      if (!pending) return;
      reloads.delete(token);
      clearTimeout(pending.timer);
      try {
        requireIdle(currentCtx);
        pi.appendEntry('pi-qq-client-reload', { token, pid: process.pid, at: Date.now() });
        reloading = true;
        pending.resolve({ status: 'reloading', onReplyFailure: () => { reloading = false; }, afterReply: () => {
          // The WebSocket result must be sent before reload closes this extension.
          // Never use the old pi/ctx after reload has invalidated it.
          void currentCtx.reload().catch(() => {});
        } });
      } catch (error) { pending.reject(error); }
    },
  });

  return {
    get reloading() { return reloading; },
    stop() {
      stopped = true;
      for (const pending of reloads.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error('The session closed before command dispatch.'));
      }
      reloads.clear();
    },
    async execute({ name, args = '', token }) {
      if (stopped) throw new Error('The session has changed.');
      if (typeof name !== 'string' || typeof args !== 'string' || args.length > 16000) throw new Error('Invalid Pi command.');
      args = args.trim();
      const ctx = getContext();
      if (name === 'commands') {
        if (!args) return { status: 'completed', view: 'controls', text: `Pi controls\nModel: ${ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : 'Not selected'}\nThinking: ${pi.getThinkingLevel()}\nChoose an action below. Text: /model, /thinking, /compact, /reload, /commands 1.` };
        const page = pageOf(commands(), args, 'commands');
        return { status: 'completed', view: 'commands', page: page.page, pages: page.pages,
          choices: page.items.map((item) => ({ label: item.name, value: item.name })),
          text: `Installed commands · Page ${page.page} of ${page.pages}\n${page.items.map((item) => `/pi ${item.name} — ${cleanLabel(item.description || item.source, 100)}`).join('\n') || 'No installed commands.'}\nTap a command to fill the composer, add arguments if needed, then send. Some commands need the Pi terminal.${page.next}` };
      }
      if (name === 'model') {
        if (!args || /^\d+$/.test(args)) {
          const available = ctx.scopedModels?.length ? ctx.scopedModels.map((item) => item.model) : ctx.modelRegistry.getAvailable();
          const page = pageOf(available, args, 'model');
          return { status: 'completed', view: 'models', page: page.page, pages: page.pages,
            choices: page.items.map((item) => ({ label: item.id, value: `${item.provider}/${item.id}`, selected: item.provider === ctx.model?.provider && item.id === ctx.model?.id })),
            text: `Model: ${ctx.model ? `${ctx.model.provider}/${ctx.model.id}` : 'Not selected'}\nAvailable models · Page ${page.page} of ${page.pages}\n${page.items.map((item) => `${item.provider}/${item.id}`).join('\n') || 'No authenticated models.'}\nSelect: /model PROVIDER/MODEL${page.next}` };
        }
        requireIdle();
        const match = /^([^\s/]+)\/(\S+)$/.exec(args);
        if (!match) throw new Error('Use /model PROVIDER/MODEL. Send /model to list available models.');
        const model = ctx.modelRegistry.find(match[1], match[2]);
        if (!model) throw new Error('That model was not found in this Pi. Send /model to list available models.');
        changing = true;
        try {
          if (!await pi.setModel(model)) throw new Error('MODEL_AUTH_MISSING');
        } catch {
          throw new Error('The model could not be selected. Check provider authentication in the Pi terminal.');
        } finally { changing = false; }
        sync();
        return { status: 'completed', view: 'controls', text: `Model: ${ctx.model.provider}/${ctx.model.id}\nThinking: ${pi.getThinkingLevel()}` };
      }
      if (name === 'thinking') {
        if (!args) return { status: 'completed', view: 'thinking', choices: thinkingLevels.map((level) => ({ label: level, value: level, selected: level === pi.getThinkingLevel() })), text: `Thinking: ${pi.getThinkingLevel()}\nChoose a level below. Levels are limited by the selected model. Text: /thinking LEVEL.` };
        requireIdle();
        if (!thinkingLevels.includes(args)) throw new Error(`Unknown thinking level. Use: ${thinkingLevels.join(', ')}.`);
        pi.setThinkingLevel(args);
        sync();
        const level = pi.getThinkingLevel();
        return { status: 'completed', view: 'thinking', choices: thinkingLevels.map((value) => ({ label: value, value, selected: value === level })), text: `Thinking: ${level}${level === args ? '' : ` (requested ${args}; limited by this model)`}` };
      }
      if (name === 'compact') {
        requireIdle();
        compacting = true;
        const finish = (ok) => {
          if (stopped) return;
          compacting = false;
          sync();
          notify(ok ? 'Compaction finished. Send /status to inspect context usage.' : 'Compaction failed. Check the Pi terminal for details before retrying.');
        };
        return { status: 'started', text: 'Compaction requested. A completion or failure notice will follow.',
          onReplyFailure: () => { compacting = false; }, afterReply: () => {
            if (stopped) return;
            try { ctx.compact({ ...(args ? { customInstructions: args } : {}), onComplete: () => finish(true), onError: () => finish(false) }); }
            catch { finish(false); }
          } };
      }
      if (name === 'reload') {
        if (args) throw new Error('Use /reload without arguments.');
        requireIdle();
        if (!/^[a-f0-9-]{36}$/.test(token || '')) throw new Error('Invalid reload request.');
        return new Promise((resolve, reject) => {
          const timer = setTimeout(() => { reloads.delete(token); reject(new Error('Reload dispatch was not confirmed. Check the Pi terminal before retrying.')); }, 4500);
          reloads.set(token, { resolve, reject, timer });
          try { pi.sendUserMessage(`/${reloadCommand} ${token}`, { expandPromptTemplates: true, deliverAs: 'followUp' }); }
          catch (error) { clearTimeout(timer); reloads.delete(token); reject(error); }
        });
      }
      if (name === 'pi') {
        const match = /^\/?([^\s/]+)(?:\s+([\s\S]*))?$/.exec(args);
        if (!match) throw new Error('Use /pi COMMAND ARGS. Send /commands for available commands.');
        if (!commands().some((item) => item.name === match[1])) throw new Error('That command is not registered in this Pi. Send /commands for available commands.');
        requireIdle();
        const text = `/${match[1]}${match[2] ? ` ${match[2]}` : ''}`;
        return { status: 'submitted', text: `Submitted /${match[1]}. Command output and dialogs may appear in the Pi terminal; task results follow normal forwarding.`, afterReply: later(() => {
          requireIdle();
          pi.sendUserMessage(text, { expandPromptTemplates: true, deliverAs: 'followUp' });
        }) };
      }
      throw new Error('Unsupported Pi command. Send /commands for available commands.');
    },
  };
}

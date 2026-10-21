import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

export async function readRequest(request) {
  let body = '';
  for await (const chunk of request) body += chunk;
  return JSON.parse(body);
}

export function sendCompletion(response, { content = '', toolCalls = [], usage }, stream = true) {
  const finishReason = toolCalls.length ? 'tool_calls' : 'stop';
  if (stream) {
    response.setHeader('Content-Type', 'text/event-stream');
    const delta = toolCalls.length
      ? { tool_calls: toolCalls.map((tool, index) => ({ index, ...tool })) }
      : { content };
    response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta }] })}\n\n`);
    response.write(`data: ${JSON.stringify({
      choices: [{ index: 0, delta: {}, finish_reason: finishReason }], usage,
    })}\n\n`);
    response.end('data: [DONE]\n\n');
  } else {
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify({
      choices: [{
        index: 0,
        message: { role: 'assistant', content, tool_calls: toolCalls },
        finish_reason: finishReason,
      }],
      usage,
    }));
  }
}

export function startJsonlSession(binary, cwd, env) {
  const child = spawn(binary, ['jsonl'], { cwd, env, stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', chunk => { stderr += chunk; });
  const deadline = setTimeout(() => child.kill('SIGKILL'), 120_000);
  const exited = new Promise(resolve => {
    child.on('exit', (code, signal) => resolve({ code, signal }));
  });
  return {
    child,
    exited,
    get stderr() { return stderr; },
    send(command) { child.stdin.write(`${JSON.stringify(command)}\n`); },
    async *messages() {
      for await (const line of createInterface({ input: child.stdout })) {
        yield JSON.parse(line);
      }
    },
    close() {
      clearTimeout(deadline);
      if (child.exitCode === null) child.kill('SIGTERM');
    },
  };
}

// Optional real interactive Pi workflows against a deterministic loopback model
// double. Isolated disposable configuration and controlled prompts only;
// no external service, credentials, or additional test dependencies.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createConnection } from "node:net";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const executable = process.env.WUMPA_PI_TEST_EXECUTABLE;
const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;

test("real interactive prompts, tools, retry, queued follow-up, cancellation, and compaction", {
    skip: !executable || process.platform !== "linux", timeout: 90000,
}, async (t) => {
    assert(isAbsolute(executable));
    const directory = await mkdtemp(join(tmpdir(), "wm-"));
    await chmod(directory, 0o700);
    const agentDir = join(directory, "agent");
    await mkdir(agentDir);
    let phase = "prompt";
    let calls = 0;
    let completed = 0;
    let cancelled = 0;
    let failed = false;
    let toolIssued = false;
    let serverError;
    const timers = new Set();
    const server = createServer(async (request, response) => {
        try {
            assert.equal(request.url, "/v1/chat/completions");
            let body = "";
            for await (const chunk of request) {
                body += chunk;
                assert(body.length <= 1024 * 1024);
            }
            assert(Array.isArray(JSON.parse(body).messages));
            calls++;
            if (phase === "retry" && !failed) {
                failed = true;
                response.writeHead(503, { "content-type": "application/json" });
                response.end(JSON.stringify({ error: { message: "Server overloaded", type: "server_error" } }));
                return;
            }
            const finished = await new Promise((resolve) => {
                const timer = setTimeout(() => {
                    timers.delete(timer);
                    resolve(true);
                }, phase === "cancel" ? 60000 : phase === "queue" ? 1200 : 600);
                timers.add(timer);
                response.once("close", () => {
                    clearTimeout(timer);
                    timers.delete(timer);
                    resolve(false);
                });
            });
            if (!finished) { cancelled++; return; }
            response.writeHead(200, { "content-type": "text/event-stream" });
            const chunk = (delta, finish_reason = null) => response.write(`data: ${JSON.stringify({
                id: "fixture-response", object: "chat.completion.chunk", created: 1, model: "fixture",
                choices: [{ index: 0, delta, finish_reason }],
            })}\n\n`);
            chunk({ role: "assistant" });
            if (phase === "tool" && !toolIssued) {
                toolIssued = true;
                chunk({ tool_calls: [{ index: 0, id: "fixture-tool", type: "function",
                    function: { name: "bash", arguments: JSON.stringify({ command: "sleep 0.6" }) } }] });
                chunk({}, "tool_calls");
            } else {
                chunk({ content: "## Summary\nDeterministic fixture response; continue the controlled test." });
                chunk({}, "stop");
            }
            response.end("data: [DONE]\n\n");
            completed++;
        } catch (error) {
            serverError = error;
            response.destroy();
        }
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    await writeFile(join(agentDir, "models.json"), JSON.stringify({ providers: { "wumpa-fixture": {
        baseUrl: `http://127.0.0.1:${server.address().port}/v1`, api: "openai-completions", apiKey: "test-only",
        models: [{ id: "fixture", reasoning: false, input: ["text"], contextWindow: 8192, maxTokens: 2048,
            cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
    } } }));
    await writeFile(join(agentDir, "settings.json"), JSON.stringify({
        enableInstallTelemetry: false, quietStartup: true,
        retry: { enabled: true, maxRetries: 2, baseDelayMs: 100, maxAgentDelayMs: 1000 },
        compaction: { enabled: false, keepRecentTokens: 16, reserveTokens: 16 },
    }));
    const id = "c".repeat(32);
    const path = join(directory, `a-${id}.activity.sock`);
    const extension = fileURLToPath(new URL("../agent-extensions/pi.ts", import.meta.url));
    const command = [executable, "--offline", "--no-approve", "--no-mcp", "--no-session",
        "--provider", "wumpa-fixture", "--model", "fixture", "--extension", extension].map(quote).join(" ");
    const child = spawn("script", ["-qefc", command, "/dev/null"], {
        cwd: directory, env: { PATH: process.env.PATH, HOME: directory, TERM: "xterm-256color",
            PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: "1", WUMPA_ACTIVITY_SOCKET: path, WUMPA_SESSION_ID: id },
        stdio: ["pipe", "pipe", "pipe"],
    });
    let exit;
    child.on("exit", (code, signal) => { exit = { code, signal }; });
    child.stdin.on("error", () => {});
    child.stdout.on("data", () => {});
    child.stderr.on("data", () => {});
    let socket;
    const records = [];
    t.after(async () => {
        socket?.destroy();
        if (exit === undefined) {
            child.stdin.write("\x1b");
            await new Promise((resolve) => setTimeout(resolve, 100));
            child.stdin.write("\x04");
            const exited = once(child, "exit");
            const timer = setTimeout(() => child.kill("SIGTERM"), 3000);
            await exited;
            clearTimeout(timer);
        }
        for (const timer of timers) clearTimeout(timer);
        server.closeAllConnections();
        await new Promise((resolve) => server.close(resolve));
        await rm(directory, { recursive: true, force: true });
    });
    async function waitFor(predicate, timeout = 15000) {
        const deadline = Date.now() + timeout;
        while (Date.now() < deadline) {
            assert.equal(exit, undefined, "Pi exited unexpectedly");
            if (serverError) throw serverError;
            if (await predicate()) return;
            await new Promise((resolve) => setTimeout(resolve, 20));
        }
        assert.fail(`workflow ${phase} timed out (calls=${calls}, completed=${completed}, cancelled=${cancelled})`);
    }
    async function subscribe() {
        await waitFor(async () => {
            if (socket) return records.length > 0;
            const candidate = createConnection(path);
            candidate.on("error", () => {});
            const connected = await new Promise((resolve) => {
                candidate.once("connect", () => resolve(true));
                candidate.once("error", () => resolve(false));
            });
            if (!connected) { candidate.destroy(); return false; }
            socket = candidate;
            socket.setEncoding("utf8");
            let buffer = "";
            socket.on("data", (chunk) => {
                buffer += chunk;
                let lf;
                while ((lf = buffer.indexOf("\n")) >= 0) {
                    records.push(JSON.parse(buffer.slice(0, lf)));
                    buffer = buffer.slice(lf + 1);
                }
            });
            socket.write(JSON.stringify({ type: "subscribe", version: 1, wumpa_session_id: id }) + "\n");
            return false;
        });
    }
    await subscribe();
    assert.equal(records.at(-1).activity, "waiting_for_input");
    await new Promise((resolve) => setTimeout(resolve, 500));
    async function run(next, input, expectedCalls, followUp = false) {
        phase = next; calls = 0; completed = 0;
        const start = records.length;
        child.stdin.write(input + "\r");
        await waitFor(() => records.slice(start).some((record) => record.activity === "working"));
        if (followUp) child.stdin.write("Controlled follow-up\x1b\r");
        if (next === "cancel") {
            await waitFor(() => calls > 0);
            child.stdin.write("\x1b");
            await waitFor(() => cancelled > 0);
        }
        await waitFor(() => calls >= expectedCalls && records.at(-1).activity === "waiting_for_input");
        const updates = records.slice(start);
        const working = updates.findIndex((record) => record.activity === "working");
        const activities = updates.slice(working).map((record) => record.activity)
            .filter((activity, index, all) => index === 0 || activity !== all[index - 1]);
        assert.deepEqual(activities, ["working", "waiting_for_input"], `${next}: false idle before settling`);
        assert.equal(updates.at(-1).wumpa_session_id, id);
    }
    await run("prompt", "Controlled ordinary prompt", 1);
    await run("tool", "Controlled tool request", 2);
    assert(toolIssued);
    await run("retry", "Controlled retry request", 2);
    assert(failed);
    await run("queue", "Controlled first queued request", 2, true);
    await run("cancel", "Controlled cancellation request", 1);
    await run("compact", "/compact", 1);
});

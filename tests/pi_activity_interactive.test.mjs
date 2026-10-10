// Optional real-Pi PTY smoke test. No prompts, credentials, or model requests.
// WUMPA_PI_TEST_EXECUTABLE=/absolute/path/to/pi node --test tests/pi_activity_interactive.test.mjs
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createConnection } from "node:net";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const executable = process.env.WUMPA_PI_TEST_EXECUTABLE;
const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;

test("interactive Pi preserves resource discovery, /name, /reload, /new, and /clone", {
    skip: !executable || process.platform !== "linux", timeout: 45000,
}, async (t) => {
    assert(isAbsolute(executable), "provide an absolute Pi executable path");
    const directory = await mkdtemp(join(tmpdir(), "wp-"));
    await chmod(directory, 0o700);
    const agentDir = join(directory, "agent");
    const extensions = join(agentDir, "extensions");
    await mkdir(extensions, { recursive: true });
    await writeFile(join(agentDir, "settings.json"), JSON.stringify({
        enableInstallTelemetry: false, quietStartup: true,
    }));
    const sentinel = join(directory, "sentinel");
    await writeFile(join(extensions, "sentinel.ts"), `
        import { appendFileSync } from "node:fs";
        export default function (pi) {
            pi.on("session_start", () => appendFileSync(${JSON.stringify(sentinel)}, "started\\n"));
            pi.registerCommand("wumpa-test-ready", {
                description: "Fixture-only terminal readiness probe",
                handler: async () => appendFileSync(${JSON.stringify(sentinel)}, "ready\\n"),
            });
        }
    `);
    const id = "b".repeat(32);
    const socketPath = join(directory, `a-${id}.activity.sock`);
    const extensionPath = fileURLToPath(new URL("../agent-extensions/pi.ts", import.meta.url));
    // script supplies a PTY to Pi. All shell inputs are controlled test paths;
    // this is not a wrapper used in production agent launching.
    const command = [executable, "--offline", "--no-approve", "--no-mcp", "--no-session",
        "--extension", extensionPath, "--name", "Wumpa initial label"].map(quote).join(" ");
    const child = spawn("script", ["-qefc", command, "/dev/null"], {
        cwd: directory,
        env: {
            PATH: process.env.PATH, HOME: directory, TERM: "xterm-256color",
            PI_CODING_AGENT_DIR: agentDir, PI_OFFLINE: "1",
            WUMPA_ACTIVITY_SOCKET: socketPath, WUMPA_SESSION_ID: id,
        },
        stdio: ["pipe", "pipe", "pipe"],
    });
    let exit;
    child.on("exit", (code, signal) => { exit = { code, signal }; });
    child.stdin.on("error", () => {});
    child.stdout.on("data", () => {}); // Drain rendering without logging transcripts.
    child.stderr.on("data", () => {});
    let socket;
    t.after(async () => {
        socket?.destroy();
        if (exit === undefined) {
            // EOF in the empty editor requests Pi's normal orderly shutdown.
            child.stdin.write("\x04");
            const exited = once(child, "exit");
            const timer = setTimeout(() => child.kill("SIGTERM"), 3000);
            await exited;
            clearTimeout(timer);
        }
        await rm(directory, { recursive: true, force: true });
    });
    async function waitFor(predicate, timeout = 10000) {
        const deadline = Date.now() + timeout;
        while (Date.now() < deadline) {
            assert.equal(exit, undefined, "interactive Pi exited unexpectedly");
            if (await predicate()) return;
            await new Promise((resolve) => setTimeout(resolve, 20));
        }
        assert.fail("interactive Pi did not report expected state before deadline");
    }
    async function subscribe() {
        const candidate = createConnection(socketPath);
        candidate.on("error", () => {});
        const connected = await new Promise((resolve) => {
            candidate.once("connect", () => resolve(true));
            candidate.once("error", () => resolve(false));
        });
        if (!connected) { candidate.destroy(); return undefined; }
        const records = [];
        let buffer = "";
        candidate.setEncoding("utf8");
        candidate.on("data", (chunk) => {
            buffer += chunk;
            let lf;
            while ((lf = buffer.indexOf("\n")) >= 0) {
                records.push(JSON.parse(buffer.slice(0, lf)));
                buffer = buffer.slice(lf + 1);
            }
        });
        candidate.write(JSON.stringify({ type: "subscribe", version: 1, wumpa_session_id: id }) + "\n");
        socket = candidate;
        return records;
    }
    let records;
    await waitFor(async () => {
        if (!records) records = await subscribe();
        return records?.length;
    });
    assert.equal(records[0].pi_session_name, "Wumpa initial label");
    assert.equal(records[0].activity, "waiting_for_input");
    assert((await readFile(sentinel, "utf8")).includes("started"));
    async function terminalReady() {
        // session_start/socket publication precedes terminal input readiness.
        // Probe a fixture-only command rather than relying on a fixed sleep.
        const count = async () => (await readFile(sentinel, "utf8")).split("ready\n").length;
        const previous = await count();
        let lastProbe = 0;
        await waitFor(async () => {
            if (await count() > previous) return true;
            if (Date.now() - lastProbe >= 500) {
                child.stdin.write("\x15/wumpa-test-ready\r");
                lastProbe = Date.now();
            }
            return false;
        }, 15000);
        await new Promise((resolve) => setTimeout(resolve, 100));
    }
    await terminalReady();
    child.stdin.write("/name Chosen by user\r");
    await waitFor(() => records.at(-1)?.pi_session_name === "Chosen by user");
    let previous = records.at(-1);
    for (const command of ["/reload", "/new", "/clone"]) {
        child.stdin.write(`${command}\r`);
        await waitFor(() => socket.closed);
        records = undefined;
        await waitFor(async () => {
            if (!records) records = await subscribe();
            return records?.length;
        });
        const snapshot = records[0];
        assert.notEqual(snapshot.generation, previous.generation);
        assert.equal(snapshot.wumpa_session_id, id);
        assert.equal(snapshot.activity, "waiting_for_input");
        if (command === "/reload") {
            assert.equal(snapshot.pi_session_id, previous.pi_session_id);
            assert.equal(snapshot.pi_session_name, "Chosen by user");
        } else {
            assert.notEqual(snapshot.pi_session_id, previous.pi_session_id);
        }
        previous = snapshot;
        await terminalReady();
    }
    assert((await readFile(sentinel, "utf8")).split("started\n").length >= 5);
});

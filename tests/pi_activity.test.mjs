// Node 22.18+ (native TypeScript stripping); no npm install or model calls.
import assert from "node:assert/strict";
import { once } from "node:events";
import { chmod, lstat, mkdtemp, readdir, readFile, rm, symlink, unlink, writeFile } from "node:fs/promises";
import { createConnection, createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import activityExtension from "../agent-extensions/pi.ts";

const ID = "a".repeat(32);
const SUBSCRIBE = JSON.stringify({ type: "subscribe", version: 1, wumpa_session_id: ID }) + "\n";

async function waitFor(predicate, timeout = 2000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
        if (await predicate()) return;
        await new Promise((resolve) => setTimeout(resolve, 10));
    }
    assert.fail("condition did not become true before deadline");
}

async function fixture(t, options = {}) {
    const directory = await mkdtemp(join(tmpdir(), "wa-"));
    await chmod(directory, 0o700);
    const path = join(directory, `a-${ID}.activity.sock`);
    const previous = [process.env.WUMPA_ACTIVITY_SOCKET, process.env.WUMPA_SESSION_ID];
    process.env.WUMPA_ACTIVITY_SOCKET = options.path ?? path;
    process.env.WUMPA_SESSION_ID = options.id ?? ID;
    const handlers = new Map();
    let name = "Initial name";
    let idle = true;
    let piId = "pi-session-one";
    const context = () => {
        // Each context is tied to an immutable session identity, as in Pi.
        const capturedId = piId;
        return { isIdle: () => idle, sessionManager: { getSessionId: () => capturedId } };
    };
    const pi = {
        on: (event, handler) => {
            assert(!handlers.has(event));
            handlers.set(event, handler);
        },
        getSessionName: () => name,
    };
    activityExtension(pi);
    const sockets = [];
    const emit = async (event, details = {}) => {
        const handler = handlers.get(event);
        if (handler) await handler({ type: event, ...details }, context());
    };
    t.after(async () => {
        await emit("session_shutdown", { reason: "quit" });
        for (const socket of sockets) socket.destroy();
        for (const [index, key] of ["WUMPA_ACTIVITY_SOCKET", "WUMPA_SESSION_ID"].entries()) {
            if (previous[index] === undefined) delete process.env[key];
            else process.env[key] = previous[index];
        }
        await rm(directory, { recursive: true, force: true });
    });
    async function connect(subscribe = SUBSCRIBE) {
        const socket = createConnection(path);
        sockets.push(socket);
        const records = [];
        let buffer = "";
        socket.on("error", () => {});
        socket.setEncoding("utf8");
        socket.on("data", (chunk) => {
            buffer += chunk;
            let lf;
            while ((lf = buffer.indexOf("\n")) >= 0) {
                records.push(JSON.parse(buffer.slice(0, lf)));
                buffer = buffer.slice(lf + 1);
            }
        });
        await once(socket, "connect");
        if (subscribe !== null) socket.write(subscribe);
        return { socket, records };
    }
    return {
        directory, path, connect, emit, handlers,
        setName: (value) => { name = value; },
        setIdle: (value) => { idle = value; },
        setPiId: (value) => { piId = value; },
        start: async () => {
            await emit("session_start", { reason: "startup" });
            await waitFor(async () => (await lstat(path).catch(() => null))?.isSocket());
        },
    };
}

// Environment belongs to each test process; keep these tests sequential.
test("factory starts no resources; disabled or invalid integration fails closed", async (t) => {
    const f = await fixture(t, { id: "invalid" });
    assert.deepEqual(await readdir(f.directory), []);
    await f.emit("session_start");
    assert.deepEqual(await readdir(f.directory), []);
    delete process.env.WUMPA_ACTIVITY_SOCKET;
    await f.emit("session_start");
    assert.deepEqual(await readdir(f.directory), []);
});

test("private publication, initial snapshot, working, settled, and name clearing", async (t) => {
    const f = await fixture(t);
    await f.start();
    assert.equal((await lstat(f.path)).mode & 0o7777, 0o600);
    const { socket, records } = await f.connect();
    await waitFor(() => records.length === 1);
    assert.deepEqual(records[0], {
        type: "status", version: 1, generation: records[0].generation, sequence: 2,
        wumpa_session_id: ID, pi_session_id: "pi-session-one",
        pi_session_name: "Initial name", activity: "waiting_for_input",
    });
    assert.match(records[0].generation, /^[0-9a-f-]{36}$/);
    // agent_start is authoritative even if an earlier handler sees idle=true.
    await f.emit("agent_start");
    await waitFor(() => records.length === 2);
    assert.equal(records[1].activity, "working");
    f.setIdle(false);
    await f.emit("agent_end");
    await f.emit("turn_end");
    assert.equal(records.length, 2);
    f.setName("Renamed\u2028conversation\n\x1b[31m");
    await f.emit("session_info_changed");
    await waitFor(() => records.length === 3);
    assert.equal(records[2].pi_session_name, "Renamed\u2028conversation\n\x1b[31m");
    assert.equal(records[2].wumpa_session_id, ID);
    f.setName(undefined);
    await f.emit("session_info_changed");
    await waitFor(() => records.length === 4);
    assert.equal(records[3].pi_session_name, null);
    f.setIdle(true);
    await f.emit("agent_settled");
    await waitFor(() => records.length === 5);
    assert.equal(records[4].activity, "waiting_for_input");
    assert(records.every((record, i) => !i || record.sequence > records[i - 1].sequence));
    const closed = once(socket, "close");
    await f.emit("session_shutdown");
    await closed;
    await waitFor(async () => (await readdir(f.directory)).length === 0);
    await f.emit("session_shutdown"); // Idempotent.
});

test("fragmented UTF-8 handshake and fresh snapshots for each subscriber", async (t) => {
    const f = await fixture(t);
    await f.start();
    const first = await f.connect(null);
    const bytes = Buffer.from(SUBSCRIBE);
    for (const byte of bytes) first.socket.write(Buffer.from([byte]));
    await waitFor(() => first.records.length === 1);
    f.setName("Changed without an event");
    const second = await f.connect();
    await waitFor(() => second.records.length === 1);
    assert.equal(second.records[0].pi_session_name, "Changed without an event");
    assert(second.records[0].sequence > first.records[0].sequence);
    // A new subscriber's initial snapshot must not update broadcast coalescing.
    await f.emit("session_info_changed");
    await waitFor(() => first.records.length === 2);
    assert.equal(first.records[1].pi_session_name, "Changed without an event");
});

test("accept an exact byte-limit frame, including fragmented multibyte UTF-8", async (t) => {
    const f = await fixture(t);
    await f.start();
    const plain = SUBSCRIBE.trimEnd();
    const padded = plain + " ".repeat(8192 - Buffer.byteLength(plain)) + "\n";
    const accepted = await f.connect(padded);
    await waitFor(() => accepted.records.length === 1);
    accepted.socket.destroy();
    await waitFor(() => accepted.socket.closed);
    // Split inside a literal multibyte value that is invalid semantically,
    // but still exercises byte decoding.
    const invalid = await f.connect(null);
    const bytes = Buffer.from(SUBSCRIBE.replace("subscribe", "🍇"));
    const split = bytes.indexOf(Buffer.from("🍇")) + 1;
    invalid.socket.write(bytes.subarray(0, split));
    await new Promise((resolve) => setTimeout(resolve, 10));
    invalid.socket.write(bytes.subarray(split));
    await waitFor(() => invalid.socket.closed);
    assert.equal(invalid.records.length, 0);
});

test("reject malformed, oversized, mismatched, unsupported, and extra requests", async (t) => {
    const f = await fixture(t);
    await f.start();
    for (const request of [
        "not json\n", "null\n", "[]\n", "x".repeat(8193),
        SUBSCRIBE.replace('"version":1', '"version":2'),
        SUBSCRIBE.replace(ID, "b".repeat(32)),
        SUBSCRIBE.trimEnd() + "\u2028", // Not LF framing.
        SUBSCRIBE + SUBSCRIBE,
        SUBSCRIBE.replace('"type":', '"extra":true,"type":'),
        Buffer.concat([Buffer.from([0xff]), Buffer.from(SUBSCRIBE)]),
    ]) {
        const { socket, records } = await f.connect(request);
        await waitFor(() => socket.closed, 4000);
        assert.equal(records.length, 0);
    }
    const accepted = await f.connect();
    await waitFor(() => accepted.records.length === 1);
    accepted.socket.write(SUBSCRIBE);
    await waitFor(() => accepted.socket.closed);
});

test("bound connections and expire incomplete handshakes", async (t) => {
    const f = await fixture(t);
    await f.start();
    const clients = [];
    for (let i = 0; i < 4; i++) clients.push(await f.connect(null));
    const excess = await f.connect(null);
    await waitFor(() => excess.socket.closed);
    await waitFor(() => clients.every(({ socket }) => socket.closed), 4000);
    const recovered = await f.connect();
    await waitFor(() => recovered.records.length === 1);
});

test("heartbeat reconciles missing events and coalesces unchanged snapshots", async (t) => {
    const f = await fixture(t);
    await f.start();
    const { records } = await f.connect();
    await waitFor(() => records.length === 1);
    await f.emit("session_info_changed");
    await f.emit("agent_settled");
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert.equal(records.length, 1);
    f.setIdle(false);
    f.setName("Recovered by heartbeat");
    await waitFor(() => records.length >= 2, 6000);
    assert.equal(records[1].activity, "working");
    assert.equal(records[1].pi_session_name, "Recovered by heartbeat");
    await waitFor(() => records.length >= 3, 6000);
    assert.equal(records[2].activity, "working");
    assert(records[2].sequence > records[1].sequence);
});

test("reload and session replacement use fresh context and generation without renaming", async (t) => {
    const f = await fixture(t);
    await f.start();
    let client = await f.connect();
    await waitFor(() => client.records.length === 1);
    let generation = client.records[0].generation;
    for (const reason of ["reload", "new", "resume", "fork"]) {
        f.setName(`Name on ${reason}`);
        f.setPiId(`pi-${reason}`);
        await f.emit("session_shutdown", { reason });
        await waitFor(() => client.socket.closed);
        await f.emit("session_start", { reason });
        await waitFor(async () => (await lstat(f.path).catch(() => null))?.isSocket());
        client = await f.connect();
        await waitFor(() => client.records.length === 1);
        assert.notEqual(client.records[0].generation, generation);
        assert.equal(client.records[0].pi_session_id, `pi-${reason}`);
        assert.equal(client.records[0].pi_session_name, `Name on ${reason}`);
        generation = client.records[0].generation;
    }
});

test("compaction and summary boundaries refresh but never declare completion", async (t) => {
    const f = await fixture(t);
    await f.start();
    const { records } = await f.connect();
    await waitFor(() => records.length === 1);
    const events = ["session_before_compact", "session_compact", "session_compact_failed",
        "session_before_tree", "session_tree"];
    for (const event of events) {
        f.setIdle(false);
        f.setName(event);
        await f.emit(event);
        await waitFor(() => records.at(-1)?.pi_session_name === event);
        assert.equal(records.at(-1).activity, "working");
    }
});

test("deferred compaction/tree samples settle promptly and are cancelled on shutdown", async (t) => {
    const f = await fixture(t);
    await f.start();
    const { records } = await f.connect();
    await waitFor(() => records.length === 1);
    for (const event of ["session_compact", "session_tree", "session_compact_failed"]) {
        const previousCount = records.length;
        f.setIdle(false);
        await f.emit(event); // Pi still exposes busy during the handler.
        f.setIdle(true); // Pi clears its controller after dispatch returns.
        await waitFor(() => records.length >= previousCount + 2);
        assert.equal(records.at(-1).activity, "waiting_for_input");
        assert.equal(records.at(-2).activity, "working");
    }
    f.setIdle(false);
    await f.emit("session_compact");
    await f.emit("session_shutdown");
    await waitFor(async () => (await readdir(f.directory)).length === 0);
    f.setPiId("replacement-session");
    f.setIdle(true);
    await f.start();
    const replacement = await f.connect();
    await waitFor(() => replacement.records.length === 1);
    assert.equal(replacement.records[0].pi_session_id, "replacement-session");
});

test("large names are bounded by Unicode characters and frame bytes", async (t) => {
    const f = await fixture(t);
    f.setName("\0".repeat(10000)); // JSON escaping is the worst case: six bytes.
    await f.start();
    const { records } = await f.connect();
    await waitFor(() => records.length === 1);
    assert.equal(records[0].pi_session_name.length, 512);
    assert(Buffer.byteLength(JSON.stringify(records[0])) <= 8192);
    f.setName("🍇".repeat(1000));
    await f.emit("session_info_changed");
    await waitFor(() => records.length === 2);
    assert.equal(Array.from(records[1].pi_session_name).length, 512);
    assert(!records[1].pi_session_name.includes("\ufffd"));
});

test("occupied files, symlinks, and listeners are never overwritten or unlinked", async (t) => {
    const f = await fixture(t);
    await writeFile(f.path, "occupied", { mode: 0o600 });
    await f.emit("session_start");
    await waitFor(async () => (await readdir(f.directory)).length === 1);
    assert.equal(await readFile(f.path, "utf8"), "occupied");
    await unlink(f.path);
    const target = join(f.directory, "target");
    await writeFile(target, "untouched", { mode: 0o600 });
    await symlink(target, f.path);
    await f.emit("session_start");
    await waitFor(async () => (await readdir(f.directory)).length === 2);
    assert((await lstat(f.path)).isSymbolicLink());
    assert.equal(await readFile(target, "utf8"), "untouched");
    await unlink(f.path);
    const other = createServer();
    await new Promise((resolve) => other.listen(f.path, resolve));
    await chmod(f.path, 0o600);
    t.after(() => other.close());
    const inode = (await lstat(f.path)).ino;
    await f.emit("session_start");
    await waitFor(async () => (await readdir(f.directory)).length === 2);
    assert.equal((await lstat(f.path)).ino, inode);
    await f.emit("session_shutdown");
    assert.equal((await lstat(f.path)).ino, inode);
});

test("shutdown preserves an endpoint replaced by another listener", async (t) => {
    const f = await fixture(t);
    await f.start();
    await unlink(f.path);
    const replacement = createServer();
    await new Promise((resolve) => replacement.listen(f.path, resolve));
    await chmod(f.path, 0o600);
    t.after(() => replacement.close());
    const identity = await lstat(f.path);
    await f.emit("session_shutdown");
    await waitFor(async () => (await readdir(f.directory)).length === 1);
    assert.equal((await lstat(f.path)).ino, identity.ino);
});

test("unsafe private directories and overlong paths disable reporting", async (t) => {
    const f = await fixture(t);
    await chmod(f.directory, 0o755);
    await f.emit("session_start");
    assert.deepEqual(await readdir(f.directory), []);
    await chmod(f.directory, 0o700);
    const alias = `${f.directory}-alias`;
    await symlink(f.directory, alias);
    t.after(() => unlink(alias));
    process.env.WUMPA_ACTIVITY_SOCKET = join(alias, `a-${ID}.activity.sock`);
    await f.emit("session_start");
    assert.deepEqual(await readdir(f.directory), []);
    process.env.WUMPA_ACTIVITY_SOCKET = join(f.directory, "x".repeat(108), `a-${ID}.activity.sock`);
    await f.emit("session_start");
    assert.deepEqual(await readdir(f.directory), []);
});

test("slow subscribers disconnect without delaying hooks or preventing reconnect", async (t) => {
    const f = await fixture(t);
    await f.start();
    const { socket, records } = await f.connect();
    await waitFor(() => records.length === 1);
    const start = Date.now();
    // All hooks run without yielding to network I/O, forcing backpressure.
    for (let i = 0; i < 1000; i++) {
        f.setName(String(i));
        f.handlers.get("session_info_changed")({}, {
            isIdle: () => false, sessionManager: { getSessionId: () => "same-session" },
        });
    }
    assert(Date.now() - start < 1000);
    await waitFor(() => socket.closed);
    const recovered = await f.connect();
    await waitFor(() => recovered.records.length === 1);
    assert.equal(recovered.records[0].pi_session_name, "999");
});

test("shutdown racing startup and snapshot errors leak no resources", async (t) => {
    const f = await fixture(t);
    for (let i = 0; i < 10; i++) {
        await f.emit("session_start");
        await f.emit("session_shutdown");
    }
    await waitFor(async () => (await readdir(f.directory)).length === 0);
    await f.start();
    f.setPiId("x".repeat(257));
    await f.emit("session_info_changed");
    await waitFor(async () => (await readdir(f.directory)).length === 0);
    f.setPiId("valid-again");
    await f.start();
    assert.doesNotThrow(() => f.handlers.get("agent_settled")({}, {
        isIdle: () => { throw new Error("context unavailable"); },
        sessionManager: { getSessionId: () => "valid-again" },
    }));
    await waitFor(async () => (await readdir(f.directory)).length === 0);
});

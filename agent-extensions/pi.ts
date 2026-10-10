// Bundled observational integration for Pi 1.0.4. No runtime dependencies.
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { randomUUID } from "node:crypto";
import {
    chmodSync, linkSync, lstatSync, mkdtempSync, realpathSync, rmdirSync, unlinkSync,
} from "node:fs";
import type { Stats } from "node:fs";
import { createServer } from "node:net";
import type { Server, Socket } from "node:net";
import { basename, dirname, isAbsolute, join } from "node:path";

const MAX_FRAME_BYTES = 8192; // Excludes the framing LF.
const MAX_CLIENTS = 4;
const MAX_NAME_CHARACTERS = 512;
const HANDSHAKE_MS = 3000;
const HEARTBEAT_MS = 5000;

interface Client {
    socket: Socket;
    buffer: Buffer;
    subscribed: boolean;
    handshake: ReturnType<typeof setTimeout>;
}

// Linux is the only supported Wumpa agent execution platform. Do not infer
// support from an executable basename or start resources in the factory.
export default function (pi: ExtensionAPI) {
    let current: ActivityServer | undefined;
    pi.on("session_start", (_event, ctx) => {
        current?.stop();
        current = undefined;
        try {
            const path = process.env.WUMPA_ACTIVITY_SOCKET;
            const id = process.env.WUMPA_SESSION_ID;
            if (process.platform !== "linux" || !path || !id) return;
            current = new ActivityServer(pi, ctx, path, id);
            current.start();
        } catch {
            current?.stop();
            current = undefined;
        }
    });
    pi.on("session_shutdown", () => {
        current?.stop();
        current = undefined;
    });
    pi.on("agent_start", (_event, ctx) => current?.refresh(ctx, "working"));
    pi.on("agent_settled", (_event, ctx) => current?.refresh(ctx));
    pi.on("session_info_changed", (_event, ctx) => current?.refresh(ctx));
    pi.on("session_before_compact", (_event, ctx) => current?.refresh(ctx));
    pi.on("session_compact", (_event, ctx) => current?.refreshAfterBoundary(ctx));
    pi.on("session_compact_failed", (_event, ctx) => current?.refreshAfterBoundary(ctx));
    pi.on("session_before_tree", (_event, ctx) => current?.refresh(ctx));
    pi.on("session_tree", (_event, ctx) => current?.refreshAfterBoundary(ctx));
    // agent_end/turn_end are deliberately not completion signals.
}

class ActivityServer {
    private server: Server;
    private clients = new Set<Client>();
    private generation = randomUUID();
    private sequence = 0;
    private context: ExtensionContext;
    private lastState: string | undefined;
    private heartbeat: ReturnType<typeof setInterval> | undefined;
    private startup: ReturnType<typeof setTimeout> | undefined;
    private deferred: ReturnType<typeof setImmediate> | undefined;
    private staging: string | undefined;
    private identity: Stats | undefined;
    private stopped = false;
    private pi: ExtensionAPI;
    private path: string;
    private id: string;

    constructor(pi: ExtensionAPI, context: ExtensionContext, path: string, id: string) {
        this.pi = pi;
        this.path = path;
        this.id = id;
        this.context = context;
        this.server = createServer((socket) => this.accept(socket));
        this.server.on("error", () => this.stop());
        this.server.unref();
    }

    start(): void {
        try {
            const parent = dirname(this.path);
            const directory = lstatSync(parent);
            if (!/^[a-f0-9]{32}$/.test(this.id)
                || !isAbsolute(this.path)
                || basename(this.path) !== `a-${this.id}.activity.sock`
                || Buffer.byteLength(this.path) > 107
                || realpathSync(parent) !== parent
                || !directory.isDirectory()
                || directory.uid !== process.geteuid?.()
                || (directory.mode & 0o7777) !== 0o700) {
                this.stop();
                return;
            }
            // Bind privately, chmod, then publish via an exclusive hard link.
            // Node closes/unlinks only its staging pathname, never a replacement
            // at the public endpoint. Occupied endpoints are never overwritten.
            this.staging = mkdtempSync(join(parent, "p-"));
            chmodSync(this.staging, 0o700);
            const temporary = join(this.staging, "s");
            this.startup = setTimeout(() => this.stop(), HANDSHAKE_MS);
            this.startup.unref();
            this.server.listen(temporary, () => {
                if (this.stopped) {
                    this.close();
                    return;
                }
                try {
                    chmodSync(temporary, 0o600);
                    this.identity = lstatSync(temporary);
                    linkSync(temporary, this.path);
                    clearTimeout(this.startup);
                    this.heartbeat = setInterval(() => this.publish(true), HEARTBEAT_MS);
                    this.heartbeat.unref();
                    this.publish(true);
                } catch {
                    this.stop();
                }
            });
        } catch {
            this.stop();
        }
    }

    refresh(context: ExtensionContext, activity?: "working"): void {
        this.context = context;
        this.publish(false, activity);
    }

    refreshAfterBoundary(context: ExtensionContext): void {
        this.refresh(context);
        // Pi clears compaction/tree busy state after these handlers return.
        // Sample on the next loop without asserting idle or retaining an old
        // session context. The heartbeat covers other async handlers/aborts.
        if (this.stopped || this.deferred) return;
        this.deferred = setImmediate(() => {
            this.deferred = undefined;
            this.publish(false);
        });
        this.deferred.unref();
    }

    private publish(heartbeat: boolean, activity?: "working", recipient?: Client): void {
        if (this.stopped || !this.identity) return;
        try {
            const name = this.pi.getSessionName();
            let boundedName: string | null = null;
            if (name !== undefined) {
                boundedName = "";
                let count = 0;
                for (const character of name) {
                    if (count++ >= MAX_NAME_CHARACTERS) break;
                    boundedName += character;
                }
            }
            // Preserve null for name-clear events. Never interpret the name as
            // an identity, command, target, or path; consumers escape for display.
            const state = {
                pi_session_id: this.context.sessionManager.getSessionId(),
                pi_session_name: boundedName,
                activity: activity ?? (this.context.isIdle() ? "waiting_for_input" : "working"),
            };
            if (typeof state.pi_session_id !== "string" || state.pi_session_id.length > 256) {
                this.stop();
                return;
            }
            const serialized = JSON.stringify(state);
            if (!heartbeat && !recipient && serialized === this.lastState) return;
            // An initial snapshot sent only to a new subscriber must not
            // suppress a subsequent broadcast to existing subscribers.
            if (!recipient) this.lastState = serialized;
            if (!Number.isSafeInteger(++this.sequence)) {
                this.stop();
                return;
            }
            const record = JSON.stringify({
                type: "status", version: 1, generation: this.generation,
                sequence: this.sequence, wumpa_session_id: this.id, ...state,
            });
            if (Buffer.byteLength(record) > MAX_FRAME_BYTES) {
                this.stop();
                return;
            }
            for (const client of recipient ? [recipient] : this.clients) {
                if (!client.subscribed) continue;
                // No awaited writes or unbounded queue in a Pi lifecycle hook.
                // A backpressured client must reconnect for a fresh snapshot.
                if (client.socket.writableLength !== 0 || !client.socket.write(`${record}\n`)) {
                    client.socket.destroy();
                }
            }
        } catch {
            this.stop();
        }
    }

    private accept(socket: Socket): void {
        socket.on("error", () => socket.destroy());
        if (this.stopped || !this.identity || this.clients.size >= MAX_CLIENTS) {
            socket.destroy();
            return;
        }
        socket.unref();
        const client: Client = {
            socket, buffer: Buffer.alloc(0), subscribed: false,
            handshake: setTimeout(() => socket.destroy(), HANDSHAKE_MS),
        };
        client.handshake.unref();
        this.clients.add(client);
        socket.on("close", () => {
            clearTimeout(client.handshake);
            this.clients.delete(client);
        });
        socket.on("data", (chunk: Buffer) => {
            // Exactly one LF-delimited subscribe record is allowed. Buffer byte
            // lengths, not string lengths or Unicode line separators, frame it.
            if (client.subscribed || client.buffer.length + chunk.length > MAX_FRAME_BYTES + 1) {
                socket.destroy();
                return;
            }
            client.buffer = Buffer.concat([client.buffer, chunk]);
            const lf = client.buffer.indexOf(10);
            if (lf < 0) {
                if (client.buffer.length > MAX_FRAME_BYTES) socket.destroy();
                return;
            }
            try {
                if (lf !== client.buffer.length - 1) throw new Error("extra data");
                const text = new TextDecoder("utf-8", { fatal: true }).decode(client.buffer.subarray(0, lf));
                const request = JSON.parse(text);
                if (request?.type !== "subscribe" || request.version !== 1
                    || request.wumpa_session_id !== this.id
                    || Object.keys(request).length !== 3) throw new Error("invalid subscription");
                client.buffer = Buffer.alloc(0);
                client.subscribed = true;
                clearTimeout(client.handshake);
                this.publish(true, undefined, client);
            } catch {
                socket.destroy();
            }
        });
    }

    stop(): void {
        if (this.stopped) return;
        this.stopped = true;
        clearTimeout(this.startup);
        clearInterval(this.heartbeat);
        if (this.deferred) clearImmediate(this.deferred);
        for (const client of this.clients) client.socket.destroy();
        this.clients.clear();
        try {
            const actual = lstatSync(this.path);
            if (this.identity && actual.isSocket()
                && actual.dev === this.identity.dev && actual.ino === this.identity.ino
                && actual.uid === this.identity.uid && (actual.mode & 0o7777) === 0o600) {
                unlinkSync(this.path);
            }
        } catch { /* Missing/replaced endpoints and reporting errors are harmless. */ }
        this.close();
    }

    private close(): void {
        // Also runs when shutdown races with the asynchronous listen callback.
        this.server.close(() => {
            if (this.staging) {
                try { rmdirSync(this.staging); } catch { /* Never remove unknown entries. */ }
            }
        });
    }
}

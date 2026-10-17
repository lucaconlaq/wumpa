// Node 22.18+; no Pi install, model calls, or running Wumpa server required.
import assert from "node:assert/strict";
import { test } from "node:test";
import activityExtension from "../agent-extensions/pi.ts";

const ID = "abcdef0123456789abcdef0123456789";

test("Wumpa footer shows only its ID without leaking inherited identity", async (t) => {
    const keys = ["WUMPA_SESSION_ID", "WUMPA_ACTIVITY_SOCKET", "PSST_SUBAGENT"];
    const previous = keys.map((key) => process.env[key]);
    t.after(() => {
        keys.forEach((key, index) => {
            if (previous[index] === undefined) delete process.env[key];
            else process.env[key] = previous[index];
        });
    });
    // No socket is needed for the local display status.
    delete process.env.WUMPA_ACTIVITY_SOCKET;
    delete process.env.PSST_SUBAGENT;
    process.env.WUMPA_SESSION_ID = ID;
    const handlers = new Map();
    const statuses = new Map();
    let name = "Fix authentication";
    activityExtension({
        on: (event, handler) => handlers.set(event, handler),
        getSessionName: () => name,
    });
    const context = {
        mode: "tui",
        ui: { setStatus: (key, value) => statuses.set(key, value) },
    };
    const emit = (event, ctx = context) => handlers.get(event)?.({ type: event }, ctx);
    const status = () => statuses.get("wumpa-session");

    await t.test("factory is inert; startup publishes the short Wumpa ID", () => {
        assert.equal(statuses.size, 0);
        emit("session_start");
        assert.equal(status(), " wumpa: abcde");
        name = "Renamed conversation";
        emit("session_info_changed");
        assert.equal(status(), " wumpa: abcde");
    });
    await t.test("conversation replacement and cleared names do not change the Wumpa ID", () => {
        name = "Resumed conversation";
        emit("session_start");
        assert.equal(status(), " wumpa: abcde");
        for (const value of [undefined, "", "  "]) {
            name = value;
            emit("session_info_changed");
            assert.equal(status(), " wumpa: abcde");
        }
    });
    await t.test("untrusted names are never included in the footer", () => {
        for (const value of ["bad\x1b[31m\nline\u2028bidi\u202e!", "🌍".repeat(81)]) {
            name = value;
            emit("session_info_changed");
            assert.equal(status(), " wumpa: abcde");
        }
    });
    await t.test("missing or malformed identity and delegated children hide the status", () => {
        for (const value of ["", "invalid", "A".repeat(32)]) {
            process.env.WUMPA_SESSION_ID = value;
            emit("session_start");
            assert.equal(status(), undefined);
        }
        delete process.env.WUMPA_SESSION_ID;
        emit("session_start");
        assert.equal(status(), undefined);
        process.env.WUMPA_SESSION_ID = ID;
        process.env.PSST_SUBAGENT = "1";
        emit("session_start");
        assert.equal(status(), undefined);
        delete process.env.PSST_SUBAGENT;
    });
    await t.test("non-interactive modes do not access UI; shutdown clears status", () => {
        for (const mode of ["json", "print", "rpc"]) {
            emit("session_start", { mode });
            emit("session_info_changed", { mode });
        }
        name = "Final name";
        emit("session_start");
        assert.equal(status(), " wumpa: abcde");
        emit("session_shutdown");
        assert.equal(status(), undefined);
        emit("session_shutdown");
    });
});

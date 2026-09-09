// IosToolHost.swift — the Swift side of the choreo-daemon iOS-native-tool
// bridge (Rust counterpart: choreo-gui/src/ios_bridge.rs, trait in
// choreo-daemon/src/tools/ios_bridge.rs).
//
// CONTRACT (BINDING — mirrored from the Rust module header):
//
//   Reply-slot ownership contract: each request boxes a one-shot crossbeam
//   reply Sender as an opaque pointer passed through the C ABI; ownership
//   transfers to Swift at dispatch; Rust NEVER frees the box; Swift
//   guarantees exactly-once reply on its serial main queue; if Rust abandons
//   (timeout/cancel) it drops the receiver and a late Swift reply sends into
//   a disconnected channel (Err ignored) and then drops the slot — no UAF,
//   no leak.
//
// Threading: `choreo_ios_tool_request` only ENQUEUES onto the main queue and
// returns immediately; the blocking wait happens Rust-side on the reply
// channel. All work and replies happen on DispatchQueue.main (serial ⇒ the
// exactly-once flag and the in-flight registry need no locking).
//
// CI status: this file is NOT compiled in CI — the first Swift error
// surfaces on a Mac. scripts/build-ios.sh's zig path validates the Rust
// cfg(ios) code for aarch64-apple-ios; the final Apple link (including this
// file) is skipped on non-Mac hosts, which is expected.
//
// Reply convention: status 0 = success (payload = JSON text, or NULL for
// `null`); nonzero = failure (payload = human-readable message, or NULL).
// The payload pointer is valid only for the duration of the callback — Rust
// copies/decodes inside the call.

import Foundation
import UIKit
import UserNotifications

// Per-request state: holds the opaque reply slot + callback and enforces the
// exactly-once reply. Created on the main queue (inside the enqueue closure)
// and only ever touched there — no synchronization needed.
final class IosToolRequestState {
    let replyCtx: UnsafeMutableRawPointer
    let replyCb: @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?) -> Void
    private var replied = false

    init(replyCtx: UnsafeMutableRawPointer,
         replyCb: @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?) -> Void) {
        self.replyCtx = replyCtx
        self.replyCb = replyCb
    }

    /// Exactly-once reply. Must be called on the main queue. `payload` is
    /// copied to a C buffer whose lifetime spans exactly the callback call —
    /// the pointer is contractually invalid once the callback returns, so it
    /// is freed right after.
    func reply(_ status: Int32, _ payload: String?) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !replied else { return }
        replied = true
        // strdup can only fail on OOM; degrade to a NULL payload (the Rust
        // side treats NULL as `null` JSON on success / generic message on
        // failure) rather than crashing the host app.
        let buf: UnsafeMutablePointer<CChar>? = payload.flatMap { strdup($0) }
        replyCb(replyCtx, status, buf.map { UnsafePointer($0) })
        if let buf = buf {
            free(buf)
        }
    }
}

enum IosToolHost {
    /// In-flight requests by id, for best-effort cancel. Main-queue only.
    private static var inflight: [UInt64: IosToolRequestState] = [:]
    /// Whether we already asked UNUserNotificationCenter for (provisional)
    /// authorization — requested lazily on first `notify`, never again.
    private static var notificationAuthRequested = false

    // ── Entry points (called by the top-level @_cdecl wrappers below) ────
    //
    // `@_cdecl` can only be applied to TOP-LEVEL global functions — Swift
    // rejects it on enum statics — so the exported C symbols are thin
    // top-level wrappers (bottom of this file) over the statics here, which
    // keep the in-flight state and the actual work.

    /// Enqueue a tool request onto the main queue. NEVER blocks and never
    /// touches UIKit directly — the caller is a Rust daemon worker thread.
    /// NOTE: `request_id` is a (documented) addition to the subsession
    /// contract's four-argument signature — without it,
    /// `choreo_ios_tool_cancel(request_id)` has no way to identify the
    /// request, making the best-effort cancel path unimplementable.
    static func enqueue(requestId: UInt64,
                        name: UnsafePointer<CChar>?,
                        argsJson: UnsafePointer<CChar>?,
                        replyCtx: UnsafeMutableRawPointer?,
                        replyCb: @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?) -> Void) {
        let nameStr = name.map { String(cString: $0) } ?? ""
        let argsStr = argsJson.map { String(cString: $0) } ?? ""
        guard let ctx = replyCtx else {
            // Contract violation on the Rust side (never expected): nothing
            // to reply into. Log-and-drop (no reply is possible at all).
            NSLog("choreo_ios_tool_request: null reply_ctx (contract violation)")
            return
        }
        DispatchQueue.main.async {
            let state = IosToolRequestState(replyCtx: ctx, replyCb: replyCb)
            IosToolHost.inflight[requestId] = state
            IosToolHost.handle(name: nameStr, argsJson: argsStr, state: state)
            // Deregister after the synchronous part: any later completion
            // (open_url / notify) still replies through the captured state,
            // but cancel no longer applies once work is underway.
            IosToolHost.inflight.removeValue(forKey: requestId)
        }
    }

    /// Best-effort cancel: if the request is still queued (not yet running),
    /// drop it and reply "canceled"; a running request finishes on its own
    /// and replies normally. Rust-side the caller's flag is authoritative
    /// either way — a reply into an abandoned (disconnected) channel is
    /// ignored and frees the slot, per the ownership contract above.
    static func cancel(requestId: UInt64) {
        DispatchQueue.main.async {
            guard let state = IosToolHost.inflight.removeValue(forKey: requestId) else {
                return // already running/completed — nothing to cancel here
            }
            state.reply(1, "request was canceled before it ran")
        }
    }

    // ── Dispatch ──────────────────────────────────────────────────────────

    /// Main-queue-serialized handler dispatch. Each handler replies exactly
    /// once through `state` (its `reply` enforces that).
    private static func handle(name: String, argsJson: String, state: IosToolRequestState) {
        // args are decoded defensively: a malformed payload is a Platform
        // error, never a crash.
        let args = argsJson.data(using: .utf8)
            .flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }
        guard let args = args else {
            state.reply(1, "tool arguments were not a JSON object")
            return
        }
        switch name {
        case "clipboard_write": handleClipboardWrite(args, state)
        case "clipboard_read": handleClipboardRead(state)
        case "open_url": handleOpenUrl(args, state)
        case "notify": handleNotify(args, state)
        default:
            state.reply(1, "unknown iOS tool: \(name)")
        }
    }

    // ── Handlers ──────────────────────────────────────────────────────────

    /// clipboard_write { text: String } → null. UIPasteboard must be touched
    /// on the main thread — we are on it by construction.
    private static func handleClipboardWrite(_ args: [String: Any], _ state: IosToolRequestState) {
        guard let text = args["text"] as? String else {
            state.reply(1, "clipboard_write requires a string 'text' argument")
            return
        }
        UIPasteboard.general.string = text
        state.reply(0, nil) // NULL payload = JSON null on the Rust side
    }

    /// clipboard_read {} → {"text": String} (empty string when the
    /// pasteboard holds no string).
    private static func handleClipboardRead(_ state: IosToolRequestState) {
        let text = UIPasteboard.general.string ?? ""
        let payload = ["text": text]
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let json = String(data: data, encoding: .utf8) else {
            state.reply(1, "failed to encode clipboard payload")
            return
        }
        state.reply(0, json)
    }

    /// open_url { url: String } → {"opened": Bool}. Restricted to https: and
    /// mailto: — an INDEPENDENT second check (the Rust tool does its own):
    /// the Swift host must be safe even if the Rust side is ever bypassed,
    /// because UIApplication.shared.open can otherwise hand arbitrary
    /// schemes (file:, javascript:, app-scheme deep links) to the system.
    private static func handleOpenUrl(_ args: [String: Any], _ state: IosToolRequestState) {
        guard let urlString = args["url"] as? String, let url = URL(string: urlString) else {
            state.reply(1, "open_url requires a valid 'url' argument")
            return
        }
        guard url.scheme == "https" || url.scheme == "mailto" else {
            state.reply(1, "open_url allows https: and mailto: URLs only")
            return
        }
        UIApplication.shared.open(url, options: [:]) { opened in
            // Completion arrives on the main queue. Reply even when the
            // system declined: the TOOL succeeded — the model wants to know
            // whether the URL actually opened.
            state.reply(0, "{\"opened\": \(opened)}")
        }
    }

    /// notify { title: String, body: String } → {"scheduled": Bool}.
    /// Provisional authorization is requested lazily on first use: provisional
    /// notifications are delivered quietly to Notification Center without a
    /// permission prompt, which is the right default for an assistant that
    /// must never nag the user for permissions mid-conversation.
    private static func handleNotify(_ args: [String: Any], _ state: IosToolRequestState) {
        guard let title = args["title"] as? String, let body = args["body"] as? String else {
            state.reply(1, "notify requires 'title' and 'body' string arguments")
            return
        }
        let center = UNUserNotificationCenter.current()
        let ensureAuthorization: (@escaping () -> Void) -> Void = { cont in
            if notificationAuthRequested {
                cont()
                return
            }
            notificationAuthRequested = true
            // Provisional + alert/sound: quiet delivery, no system prompt.
            center.requestAuthorization(options: [.alert, .sound, .provisional]) { _, error in
                if let error = error {
                    NSLog("IosToolHost: notification authorization error: \(error)")
                }
                cont()
            }
        }
        ensureAuthorization {
            let content = UNMutableNotificationContent()
            content.title = title
            content.body = body
            let request = UNNotificationRequest(
                identifier: UUID().uuidString, content: content, trigger: nil)
            center.add(request) { error in
                // UNUserNotificationCenter callbacks arrive on an internal
                // queue — hop back to the main queue for the reply (the
                // exactly-once flag and the reply contract require it).
                DispatchQueue.main.async {
                    if let error = error {
                        state.reply(1, "failed to schedule notification: \(error.localizedDescription)")
                    } else {
                        state.reply(0, "{\"scheduled\": true}")
                    }
                }
            }
        }
    }
}

// ── C ABI entry points (top level — `@_cdecl` rejects enum statics) ────────
// Thin wrappers over IosToolHost.enqueue/cancel; the symbol names are the
// contract the Rust side declares (choreo-gui/src/ios_bridge.rs).

@_cdecl("choreo_ios_tool_request")
public func choreo_ios_tool_request(request_id: UInt64,
                                    name: UnsafePointer<CChar>?,
                                    args_json: UnsafePointer<CChar>?,
                                    reply_ctx: UnsafeMutableRawPointer?,
                                    reply_cb: @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafePointer<CChar>?) -> Void) {
    IosToolHost.enqueue(requestId: request_id,
                        name: name,
                        argsJson: args_json,
                        replyCtx: reply_ctx,
                        replyCb: reply_cb)
}

@_cdecl("choreo_ios_tool_cancel")
public func choreo_ios_tool_cancel(request_id: UInt64) {
    IosToolHost.cancel(requestId: request_id)
}

/*
 * Liminal Launcher — Spike #2 test client.
 *
 * Pretends to be the IPC client that will eventually live inside the
 * Minecraft hub/survival mods. The Rust launcher spawns this as a child
 * process and exchanges JSON messages with it over a localhost WebSocket
 * to validate the full lifecycle: connect, authenticate, exchange a few
 * test messages, shut down on command, exit cleanly with no orphan.
 *
 * Pure Java — no third-party libraries. Uses java.net.http.WebSocket
 * (Java 11+) and a hand-rolled regex JSON extractor sufficient for the
 * narrow set of messages this spike sends. The production mod will use
 * Gson/Jackson, both already on the Minecraft classpath.
 *
 * Usage (driven by the Rust spike, not run manually):
 *   java -Dliminal.connect.address=ws://127.0.0.1:PORT \
 *        -Dliminal.auth.token=TOKEN \
 *        TestClient
 *
 * Compile once with: javac TestClient.java
 */

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.WebSocket;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class TestClient {

    /** Bumped any time the wire format changes incompatibly. Must match the launcher. */
    private static final int PROTOCOL_VERSION = 1;

    /** Hard cap on how long we'll wait for the launcher to send shutdown. */
    private static final long IDLE_TIMEOUT_SECONDS = 60;

    public static void main(String[] args) throws Exception {
        String url = System.getProperty("liminal.connect.address");
        String token = System.getProperty("liminal.auth.token");
        if (url == null || token == null) {
            System.err.println(
                "[client] missing -Dliminal.connect.address or -Dliminal.auth.token; refusing to start"
            );
            System.exit(2);
        }
        System.out.println("[client] starting; connect=" + url);

        // The latch is signalled when we've received `shutdown` (or an
        // unrecoverable error) and the main thread should proceed to exit.
        final CountDownLatch done = new CountDownLatch(1);

        // The async WebSocket API can deliver text payloads in chunks;
        // accumulate until `last == true`, then dispatch the full message.
        final StringBuilder partial = new StringBuilder();

        // Tracks whether we're shutting down for a normal reason (received
        // shutdown) vs an error (auth rejected, parse failure, etc.).
        final ErrorFlag err = new ErrorFlag();

        HttpClient http = HttpClient.newHttpClient();
        WebSocket ws = http.newWebSocketBuilder()
            .buildAsync(URI.create(url), new WebSocket.Listener() {

                @Override
                public void onOpen(WebSocket socket) {
                    System.out.println("[client] websocket open; sending auth");
                    String authJson = String.format(
                        "{\"type\":\"auth\",\"token\":\"%s\",\"protocol_version\":%d}",
                        jsonEscape(token), PROTOCOL_VERSION
                    );
                    socket.sendText(authJson, true);
                    // Initial demand: we need this or onText won't be invoked.
                    socket.request(1);
                }

                @Override
                public CompletionStage<?> onText(WebSocket socket, CharSequence data, boolean last) {
                    partial.append(data);
                    if (last) {
                        String msg = partial.toString();
                        partial.setLength(0);
                        handleMessage(socket, msg, done, err);
                    }
                    // Request the next message — without this, demand goes
                    // to zero and the WebSocket stops delivering.
                    socket.request(1);
                    return null;
                }

                @Override
                public CompletionStage<?> onClose(WebSocket socket, int statusCode, String reason) {
                    System.out.println(
                        "[client] websocket closed by peer: " + statusCode + " " + reason
                    );
                    done.countDown();
                    return null;
                }

                @Override
                public void onError(WebSocket socket, Throwable error) {
                    System.err.println("[client] websocket error: " + error);
                    err.set();
                    done.countDown();
                }
            })
            .get();

        // Wait for the launcher to send shutdown, or for a hard timeout.
        if (!done.await(IDLE_TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
            System.err.println(
                "[client] timeout: no shutdown within " + IDLE_TIMEOUT_SECONDS + "s"
            );
            System.exit(3);
        }

        // Best-effort graceful close. The peer may have already closed.
        try {
            ws.sendClose(WebSocket.NORMAL_CLOSURE, "shutdown").get(2, TimeUnit.SECONDS);
        } catch (Exception ignored) {
            // Already closed or never opened — fine, we're exiting either way.
        }

        if (err.isSet()) {
            System.err.println("[client] exit with error");
            System.exit(1);
        }
        System.out.println("[client] exit clean");
        System.exit(0);
    }

    private static void handleMessage(
        WebSocket socket, String json, CountDownLatch done, ErrorFlag err
    ) {
        String type = jsonField(json, "type");
        System.out.println("[client] ← " + json);
        if (type == null) {
            System.err.println("[client] missing 'type' field; dropping message");
            return;
        }
        switch (type) {
            case "auth_ok":
                System.out.println("[client] auth ok");
                break;

            case "auth_rejected": {
                String reason = jsonField(json, "reason");
                System.err.println("[client] AUTH REJECTED: " + reason);
                err.set();
                done.countDown();
                break;
            }

            case "echo": {
                String payload = jsonField(json, "payload");
                if (payload == null) payload = "";
                String reply = String.format(
                    "{\"type\":\"echo_reply\",\"payload\":\"%s\"}",
                    jsonEscape(payload)
                );
                System.out.println("[client] → " + reply);
                socket.sendText(reply, true);
                break;
            }

            case "shutdown": {
                String reason = jsonField(json, "reason");
                System.out.println(
                    "[client] received shutdown (" + reason + "); sending bye"
                );
                socket.sendText("{\"type\":\"bye\"}", true);
                done.countDown();
                break;
            }

            default:
                System.err.println("[client] unknown message type: " + type);
        }
    }

    /**
     * Extract a string-typed field from a flat JSON object. Handles basic
     * backslash-escaped quotes inside the value. Will not handle nested
     * objects, arrays, numbers, booleans, etc. — sufficient for this
     * spike's protocol; not for production.
     */
    private static String jsonField(String json, String name) {
        Pattern p = Pattern.compile(
            "\"" + Pattern.quote(name) + "\"\\s*:\\s*\"((?:[^\"\\\\]|\\\\.)*)\""
        );
        Matcher m = p.matcher(json);
        if (!m.find()) return null;
        return m.group(1)
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
    }

    private static String jsonEscape(String s) {
        return s.replace("\\", "\\\\").replace("\"", "\\\"");
    }

    /** Tiny mutable boolean usable from inside lambdas/anonymous classes. */
    private static final class ErrorFlag {
        private volatile boolean set = false;
        void set() { set = true; }
        boolean isSet() { return set; }
    }
}

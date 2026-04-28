package com.liminal.hub;

import com.mojang.logging.LogUtils;
import org.slf4j.Logger;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.WebSocket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.atomic.AtomicReference;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * WebSocket-based IPC client to the Liminal Launcher.
 *
 * <p>Discovery: reads <code>%APPDATA%\Liminal\ipc.json</code> on startup.
 * That file is written by the launcher when its IPC server is up; if it's
 * missing, we assume the launcher isn't running and skip IPC entirely
 * (Minecraft still works, just no transition support). Production
 * launcher will inject <code>-Dliminal.connect.address</code> and
 * <code>-Dliminal.auth.token</code> system properties when it owns the
 * JVM launch (Milestone 0/1) — at that point this file-based discovery
 * becomes a fallback for development.
 *
 * <p>Protocol matches Spike #2's tagged JSON: snake_case keys,
 * <code>type</code> field, <code>protocol_version</code> on the auth
 * message. Hand-rolled JSON for the small set of message shapes we use;
 * the production mod will switch to Gson (already on the Minecraft
 * classpath) once we have more message types.
 */
public class IpcClient {

    private static final Logger LOG = LogUtils.getLogger();
    private static final int PROTOCOL_VERSION = 1;

    private final AtomicReference<WebSocket> socket = new AtomicReference<>();
    private volatile String authToken;

    public void startInBackground() {
        Thread t = new Thread(this::connect, "liminal-hub-ipc");
        t.setDaemon(true);
        t.start();
    }

    private void connect() {
        Path ipcInfo = locateIpcInfoFile();
        if (!Files.exists(ipcInfo)) {
            LOG.warn("[liminal_hub IPC] {} not found — launcher not running, skipping IPC",
                ipcInfo);
            return;
        }

        String url;
        String token;
        try {
            String content = Files.readString(ipcInfo);
            url = jsonField(content, "url");
            token = jsonField(content, "token");
        } catch (Exception e) {
            LOG.error("[liminal_hub IPC] failed to read {}: {}", ipcInfo, e.toString());
            return;
        }
        if (url == null || token == null) {
            LOG.error("[liminal_hub IPC] {} missing 'url' or 'token' field", ipcInfo);
            return;
        }

        this.authToken = token;
        LOG.info("[liminal_hub IPC] connecting to {}", url);

        HttpClient.newHttpClient()
            .newWebSocketBuilder()
            .buildAsync(URI.create(url), new Listener())
            .whenComplete((ws, err) -> {
                if (err != null) {
                    LOG.error("[liminal_hub IPC] connect failed: {}", err.toString());
                } else {
                    socket.set(ws);
                }
            });
    }

    /**
     * Sends {@code {"type":"transition_request","target":"<target>"}} to
     * the launcher. Quietly drops if not connected — no point queueing.
     */
    public void sendTransitionRequest(String target) {
        WebSocket ws = socket.get();
        if (ws == null) {
            LOG.warn("[liminal_hub IPC] not connected; dropping transition_request to {}", target);
            return;
        }
        String json = String.format(
            "{\"type\":\"transition_request\",\"target\":\"%s\"}", jsonEscape(target));
        LOG.info("[liminal_hub IPC] -> {}", json);
        ws.sendText(json, true);
    }

    private static Path locateIpcInfoFile() {
        // %APPDATA%\Liminal\ipc.json on Windows.
        String appdata = System.getenv("APPDATA");
        if (appdata != null) return Paths.get(appdata, "Liminal", "ipc.json");
        // Fallback for non-Windows (development on macOS/Linux).
        return Paths.get(System.getProperty("user.home"), ".liminal", "ipc.json");
    }

    private class Listener implements WebSocket.Listener {

        // The async API can deliver text in chunks; accumulate until
        // last == true, then dispatch.
        private final StringBuilder partial = new StringBuilder();

        @Override
        public void onOpen(WebSocket s) {
            LOG.info("[liminal_hub IPC] websocket open; sending auth");
            String auth = String.format(
                "{\"type\":\"auth\",\"token\":\"%s\",\"protocol_version\":%d}",
                jsonEscape(authToken), PROTOCOL_VERSION);
            s.sendText(auth, true);
            s.request(1);
        }

        @Override
        public CompletionStage<?> onText(WebSocket s, CharSequence data, boolean last) {
            partial.append(data);
            if (last) {
                String msg = partial.toString();
                partial.setLength(0);
                handleMessage(s, msg);
            }
            s.request(1);
            return null;
        }

        @Override
        public CompletionStage<?> onClose(WebSocket s, int code, String reason) {
            LOG.info("[liminal_hub IPC] websocket closed: {} {}", code, reason);
            socket.set(null);
            return null;
        }

        @Override
        public void onError(WebSocket s, Throwable t) {
            LOG.error("[liminal_hub IPC] websocket error: {}", t.toString());
            socket.set(null);
        }
    }

    private void handleMessage(WebSocket s, String json) {
        String type = jsonField(json, "type");
        LOG.info("[liminal_hub IPC] <- {}", json);
        if (type == null) return;
        switch (type) {
            case "auth_ok":
                LOG.info("[liminal_hub IPC] auth ok; sending ready");
                s.sendText("{\"type\":\"ready\",\"instance_name\":\"hub\"}", true);
                break;

            case "auth_rejected":
                LOG.error("[liminal_hub IPC] AUTH REJECTED: {}", jsonField(json, "reason"));
                break;

            // Stubs for messages we'll handle in later iterations:
            case "shutdown":
            case "show_window":
                LOG.info("[liminal_hub IPC] received {} (no handler yet)", type);
                break;

            default:
                LOG.info("[liminal_hub IPC] unhandled message type: {}", type);
        }
    }

    /** Naive single-string-field JSON extractor; matches spike-grade quality. */
    private static String jsonField(String json, String name) {
        Pattern p = Pattern.compile(
            "\"" + Pattern.quote(name) + "\"\\s*:\\s*\"((?:[^\"\\\\]|\\\\.)*)\"");
        Matcher m = p.matcher(json);
        if (!m.find()) return null;
        return m.group(1).replace("\\\"", "\"").replace("\\\\", "\\");
    }

    private static String jsonEscape(String s) {
        return s.replace("\\", "\\\\").replace("\"", "\\\"");
    }
}

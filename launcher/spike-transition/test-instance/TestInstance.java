/*
 * Liminal Launcher — Spike #3 test instance.
 *
 * Stand-in for a real Minecraft instance: opens a Swing JFrame with a
 * solid colored background and a big name label, connects to the
 * launcher's IPC, reports "ready" once its window is constructed,
 * honors show_window and shutdown messages.
 *
 * Two of these run during the spike, one at a time — first as "A"
 * (visible from launch, blue), then as "B" (launched hidden, red,
 * positioned at A's old rect, revealed on show_window). The user
 * watches for blue → loading-screen overlay → red, with no perceptible
 * window-management churn between.
 *
 * System properties (all -D flags set by the launcher):
 *   liminal.connect.address  ws://127.0.0.1:PORT          (required)
 *   liminal.auth.token        random per-launch token       (required)
 *   liminal.instance.name     "A" / "B" / etc.              (default "TestInstance")
 *   liminal.instance.color    red / blue / green / purple   (default gray)
 *   liminal.window.x          int                           (default 100)
 *   liminal.window.y          int                           (default 100)
 *   liminal.window.width      int                           (default 800)
 *   liminal.window.height     int                           (default 600)
 *   liminal.window.hidden     "true" / "false"              (default false)
 *
 * Compile once with: javac TestInstance.java
 */

import java.awt.Color;
import java.awt.Font;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.WebSocket;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import javax.swing.JFrame;
import javax.swing.JLabel;
import javax.swing.SwingConstants;
import javax.swing.SwingUtilities;

public class TestInstance {

    private static final int PROTOCOL_VERSION = 1;
    private static final long IDLE_TIMEOUT_SECONDS = 120;

    private static volatile JFrame frame;
    private static volatile String name;

    public static void main(String[] args) throws Exception {
        String connectAddr = required("liminal.connect.address");
        String token = required("liminal.auth.token");
        name = sys("liminal.instance.name", "TestInstance");
        String colorName = sys("liminal.instance.color", "gray");
        boolean hidden = "true".equalsIgnoreCase(sys("liminal.window.hidden", "false"));
        int wx = parseIntProp("liminal.window.x", 100);
        int wy = parseIntProp("liminal.window.y", 100);
        int ww = parseIntProp("liminal.window.width", 800);
        int wh = parseIntProp("liminal.window.height", 600);

        log("starting; connect=" + connectAddr + " color=" + colorName
            + " hidden=" + hidden + " bounds=(" + wx + "," + wy + ") " + ww + "x" + wh);

        // Build the frame on the EDT, blocking until done.
        Color bg = parseColor(colorName);
        SwingUtilities.invokeAndWait(() -> {
            frame = new JFrame("Liminal Test Instance " + name);
            // EXIT_ON_CLOSE so closing the window also tears the JVM down,
            // which will trip the launcher's "child exited unexpectedly"
            // path. For the happy path we exit via shutdown message.
            frame.setDefaultCloseOperation(JFrame.EXIT_ON_CLOSE);
            frame.setBounds(wx, wy, ww, wh);
            frame.setUndecorated(false);
            frame.getContentPane().setBackground(bg);

            JLabel label = new JLabel(name, SwingConstants.CENTER);
            label.setForeground(Color.WHITE);
            label.setFont(new Font("SansSerif", Font.BOLD, 240));
            frame.add(label);

            // setVisible(false) is the default, but be explicit.
            frame.setVisible(!hidden);
        });
        log("frame built (visible=" + !hidden + ")");

        final CountDownLatch done = new CountDownLatch(1);
        final StringBuilder partial = new StringBuilder();
        final ErrorFlag err = new ErrorFlag();

        HttpClient http = HttpClient.newHttpClient();
        WebSocket socket = http.newWebSocketBuilder()
            .buildAsync(URI.create(connectAddr), new WebSocket.Listener() {

                @Override
                public void onOpen(WebSocket s) {
                    log("websocket open; sending auth");
                    String authJson = String.format(
                        "{\"type\":\"auth\",\"token\":\"%s\",\"protocol_version\":%d}",
                        jsonEscape(token), PROTOCOL_VERSION
                    );
                    s.sendText(authJson, true);
                    s.request(1);
                }

                @Override
                public CompletionStage<?> onText(WebSocket s, CharSequence data, boolean last) {
                    partial.append(data);
                    if (last) {
                        String msg = partial.toString();
                        partial.setLength(0);
                        handleMessage(s, msg, done, err);
                    }
                    s.request(1);
                    return null;
                }

                @Override
                public CompletionStage<?> onClose(WebSocket s, int code, String reason) {
                    log("websocket closed by peer: " + code + " " + reason);
                    done.countDown();
                    return null;
                }

                @Override
                public void onError(WebSocket s, Throwable t) {
                    err("websocket error: " + t);
                    err.set();
                    done.countDown();
                }
            })
            .get();

        if (!done.await(IDLE_TIMEOUT_SECONDS, TimeUnit.SECONDS)) {
            err("timeout: no shutdown within " + IDLE_TIMEOUT_SECONDS + "s");
            System.exit(3);
        }

        try {
            socket.sendClose(WebSocket.NORMAL_CLOSURE, "shutdown").get(2, TimeUnit.SECONDS);
        } catch (Exception ignored) { /* already closed */ }

        // Tear down the window so the JVM can exit cleanly. Without dispose,
        // the EDT keeps the JVM alive even after main() returns.
        SwingUtilities.invokeLater(() -> {
            if (frame != null) frame.dispose();
        });

        if (err.isSet()) {
            err("exit with error");
            System.exit(1);
        }
        log("exit clean");
        System.exit(0);
    }

    private static void handleMessage(
        WebSocket s, String json, CountDownLatch done, ErrorFlag err
    ) {
        String type = jsonField(json, "type");
        log("← " + json);
        if (type == null) {
            err("missing 'type' field; dropping");
            return;
        }
        switch (type) {
            case "auth_ok":
                log("auth ok; sending ready");
                String readyJson = String.format(
                    "{\"type\":\"ready\",\"instance_name\":\"%s\"}",
                    jsonEscape(name)
                );
                s.sendText(readyJson, true);
                break;

            case "auth_rejected": {
                err("AUTH REJECTED: " + jsonField(json, "reason"));
                err.set();
                done.countDown();
                break;
            }

            case "show_window":
                log("received show_window; revealing frame");
                SwingUtilities.invokeLater(() -> {
                    if (frame != null) {
                        frame.setVisible(true);
                        frame.toFront();
                    }
                });
                break;

            case "shutdown": {
                String reason = jsonField(json, "reason");
                log("received shutdown (" + reason + "); sending bye");
                s.sendText("{\"type\":\"bye\"}", true);
                done.countDown();
                break;
            }

            default:
                err("unknown message type: " + type);
        }
    }

    // ---------- Property helpers ----------

    private static String required(String key) {
        String v = System.getProperty(key);
        if (v == null) {
            System.err.println("[?] missing required -D" + key);
            System.exit(2);
        }
        return v;
    }

    private static String sys(String key, String def) {
        String v = System.getProperty(key);
        return v == null ? def : v;
    }

    private static int parseIntProp(String key, int def) {
        try {
            return Integer.parseInt(System.getProperty(key, String.valueOf(def)));
        } catch (NumberFormatException e) {
            return def;
        }
    }

    private static Color parseColor(String s) {
        switch (s.toLowerCase()) {
            case "red":    return new Color(180, 40, 40);
            case "blue":   return new Color(40, 80, 180);
            case "green":  return new Color(40, 140, 60);
            case "purple": return new Color(120, 40, 140);
            case "orange": return new Color(220, 130, 40);
            default:       return Color.DARK_GRAY;
        }
    }

    // ---------- JSON helpers (naive; sufficient for spike messages) ----------

    private static String jsonField(String json, String fieldName) {
        Pattern p = Pattern.compile(
            "\"" + Pattern.quote(fieldName) + "\"\\s*:\\s*\"((?:[^\"\\\\]|\\\\.)*)\""
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

    // ---------- Misc ----------

    private static void log(String s) {
        System.out.println("[" + (name == null ? "?" : name) + "] " + s);
    }

    private static void err(String s) {
        System.err.println("[" + (name == null ? "?" : name) + "] " + s);
    }

    private static final class ErrorFlag {
        private volatile boolean set = false;
        void set() { set = true; }
        boolean isSet() { return set; }
    }
}

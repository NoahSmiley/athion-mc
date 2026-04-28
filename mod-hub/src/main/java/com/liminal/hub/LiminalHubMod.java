package com.liminal.hub;

import com.mojang.blaze3d.platform.InputConstants;
import com.mojang.logging.LogUtils;
import net.minecraft.client.KeyMapping;
import net.neoforged.api.distmarker.Dist;
import net.neoforged.bus.api.IEventBus;
import net.neoforged.bus.api.SubscribeEvent;
import net.neoforged.fml.ModContainer;
import net.neoforged.fml.common.EventBusSubscriber;
import net.neoforged.fml.common.Mod;
import net.neoforged.neoforge.client.event.ClientTickEvent;
import net.neoforged.neoforge.client.event.RegisterKeyMappingsEvent;
import net.neoforged.neoforge.common.NeoForge;
import org.lwjgl.glfw.GLFW;
import org.slf4j.Logger;

/**
 * Liminal Launcher's hub-side mod entry point.
 *
 * <p>Responsibilities (current iteration):
 * <ul>
 *   <li>Connect to the launcher's IPC WebSocket on Minecraft startup
 *       (background thread, won't block the game if launcher isn't running).</li>
 *   <li>Register a debug key (F9) that sends a {@code transition_request}
 *       to the launcher — stand-in for the eventual server-driven trigger.</li>
 * </ul>
 *
 * <p>Future iterations will add:
 * <ul>
 *   <li>{@code liminal:transition} plugin message channel registration so
 *       the Velocity backend can trigger transitions instead of the user
 *       pressing F9.</li>
 *   <li>{@code world_loaded} signal once the player is actually in the
 *       world, replacing the launcher's fixed POST_WORLD_LOAD_DELAY_SECS.</li>
 *   <li>Loading-progress signals from Fabric/NeoForge mod-loading hooks.</li>
 * </ul>
 *
 * <p>Marked {@code dist = Dist.CLIENT} because this mod runs only inside
 * the player's Minecraft client — never on a server.
 */
@Mod(value = LiminalHubMod.MODID, dist = Dist.CLIENT)
public class LiminalHubMod {

    public static final String MODID = "liminal_hub";
    public static final Logger LOGGER = LogUtils.getLogger();

    /** Debug hotkey: send a transition_request to the launcher. */
    public static final KeyMapping TRANSITION_KEY = new KeyMapping(
        "key.liminal_hub.transition",
        InputConstants.Type.KEYSYM,
        GLFW.GLFW_KEY_F9,
        "key.categories.liminal_hub"
    );

    /** Singleton IPC client. Null until {@link #startIpc()} runs. */
    private static IpcClient IPC = null;

    public LiminalHubMod(IEventBus modEventBus, ModContainer container) {
        LOGGER.info("[liminal_hub] constructing (mod_id={}, dist=CLIENT)", MODID);

        // Hook our event handlers.
        modEventBus.addListener(this::onRegisterKeys);
        NeoForge.EVENT_BUS.register(LiminalHubMod.class);

        // Kick off IPC connection in the background. If the launcher isn't
        // running (no ipc.json), the client logs a warning and gives up
        // — no exception bubbles to crash the game.
        startIpc();
    }

    private void startIpc() {
        IPC = new IpcClient();
        IPC.startInBackground();
    }

    private void onRegisterKeys(RegisterKeyMappingsEvent event) {
        event.register(TRANSITION_KEY);
        LOGGER.info("[liminal_hub] registered debug key F9 (transition)");
    }

    /**
     * Polled once per client tick. {@link KeyMapping#consumeClick()}
     * returns true exactly once per press, so we don't need our own
     * edge-detection.
     */
    @SubscribeEvent
    public static void onClientTickPost(ClientTickEvent.Post event) {
        while (TRANSITION_KEY.consumeClick()) {
            LOGGER.info("[liminal_hub] F9 pressed — sending transition_request");
            if (IPC != null) {
                IPC.sendTransitionRequest("survival");
            } else {
                LOGGER.warn("[liminal_hub] IPC client null; can't send transition");
            }
        }
    }
}

package jp.houlab.mochidsuki.moymoy;

import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.entity.player.Inventory;
import net.minecraft.world.item.Item;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.item.Items;

/**
 * In-world emerald accounting for MoyMoy charges. 9 エメ = 1 emerald block
 * (Minecraft). All methods MUST be called on the server thread (they read and
 * mutate the player's inventory).
 */
public final class EmeraldUtil {

    private EmeraldUtil() {}

    /** {@code [emeralds, blocks]} currently in the player's inventory. */
    public static int[] count(ServerPlayer p) {
        Inventory inv = p.getInventory();
        int emeralds = 0;
        int blocks = 0;
        for (int i = 0; i < inv.getContainerSize(); i++) {
            ItemStack s = inv.getItem(i);
            if (s.is(Items.EMERALD)) {
                emeralds += s.getCount();
            } else if (s.is(Items.EMERALD_BLOCK)) {
                blocks += s.getCount();
            }
        }
        return new int[] { emeralds, blocks };
    }

    /** Total chargeable エメ ({@code emeralds + blocks*9}). */
    public static int chargeable(ServerPlayer p) {
        int[] c = count(p);
        return c[0] + c[1] * 9;
    }

    /**
     * Consume {@code amount} エメ worth of emeralds (clamped to what the player
     * holds), repacking the remainder as {@code floor(rem/9)} blocks +
     * {@code rem%9} emeralds (mirrors the design's charge math). Returns the
     * amount actually consumed. Must run on the server thread.
     */
    public static int consume(ServerPlayer p, int amount) {
        int[] c = count(p);
        int available = c[0] + c[1] * 9;
        int consumed = Math.min(Math.max(0, amount), available);
        if (consumed <= 0) {
            return 0;
        }
        int remaining = available - consumed;

        Inventory inv = p.getInventory();
        // Remove every emerald / emerald-block stack...
        for (int i = 0; i < inv.getContainerSize(); i++) {
            ItemStack s = inv.getItem(i);
            if (s.is(Items.EMERALD) || s.is(Items.EMERALD_BLOCK)) {
                inv.setItem(i, ItemStack.EMPTY);
            }
        }
        // ...then give back the repacked remainder.
        give(p, Items.EMERALD_BLOCK, remaining / 9);
        give(p, Items.EMERALD, remaining % 9);
        inv.setChanged();
        return consumed;
    }

    private static void give(ServerPlayer p, Item item, int total) {
        if (total <= 0) {
            return;
        }
        int max = Math.max(1, new ItemStack(item).getMaxStackSize());
        int rem = total;
        while (rem > 0) {
            int n = Math.min(rem, max);
            ItemStack stack = new ItemStack(item, n);
            if (!p.getInventory().add(stack)) {
                p.drop(stack, false); // inventory full → drop at the player
            }
            rem -= n;
        }
    }
}

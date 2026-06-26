/* global React */
// Reusable crystal cluster — clip-path facets with multiply blend.
// Used at corners of glass panels, buttons, app icons.
// (verbatim from Claude Design "MochiOS Mobile.html" — src/crystal.jsx)

const CRYSTAL_PALETTES = {
  red:       ["#E32636", "#FF80AB", "#8E44AD"],
  blue:      ["#3B82F6", "#4DD0E1", "#2C3E50"],
  yellow:    ["#FFD700", "#E67E22", "#D4E157"],
  green:     ["#A4C639", "#2ECC71", "#1ABC9C"],
  purple:    ["#8E44AD", "#FF80AB", "#2C3E50"],
  orange:    ["#E67E22", "#FFD700", "#E32636"],
  emerald:   ["#2ECC71", "#A4C639", "#1ABC9C"],
  turquoise: ["#1ABC9C", "#4DD0E1", "#3B82F6"],
  pink:      ["#FF80AB", "#E32636", "#8E44AD"],
  ice:       ["#4DD0E1", "#3B82F6", "#FFFFFF"],
  meadow:    ["#D4E157", "#A4C639", "#FFD700"],
  storm:     ["#2C3E50", "#3B82F6", "#8E44AD"],
};

// Standalone crystal icon — for app icons / list rows.
function CrystalIcon({ palette = "red", size = 56, glyph = "" }) {
  const colors = CRYSTAL_PALETTES[palette] || CRYSTAL_PALETTES.red;
  return (
    <div style={{
      width: size, height: size, position: "relative", flexShrink: 0,
      filter: "drop-shadow(2px 4px 0 rgba(0,0,0,0.15))",
    }}>
      {/* base diamond */}
      <div style={{
        position: "absolute", inset: 0,
        background: colors[0],
        clipPath: "polygon(50% 0%, 100% 35%, 85% 100%, 15% 100%, 0% 35%)",
        mixBlendMode: "multiply",
        boxShadow: "inset 1px 1px 12px rgba(255,255,255,0.4)",
      }} />
      <div style={{
        position: "absolute", inset: 0,
        background: colors[1],
        clipPath: "polygon(50% 0%, 100% 35%, 50% 60%, 0% 35%)",
        mixBlendMode: "multiply",
        opacity: 0.9,
      }} />
      <div style={{
        position: "absolute", inset: 0,
        background: colors[2],
        clipPath: "polygon(50% 60%, 100% 35%, 85% 100%)",
        mixBlendMode: "multiply",
        opacity: 0.8,
      }} />
      {/* gloss */}
      <div style={{
        position: "absolute", inset: 0,
        background: "linear-gradient(135deg, rgba(255,255,255,0.6) 0%, transparent 45%, rgba(0,0,0,0.15) 100%)",
        clipPath: "polygon(50% 0%, 100% 35%, 85% 100%, 15% 100%, 0% 35%)",
        pointerEvents: "none",
      }} />
      {glyph && (
        <div style={{
          position: "absolute", inset: 0, display: "grid", placeItems: "center",
          fontFamily: "var(--font-mono)", fontWeight: 700, fontSize: size * 0.32,
          color: "#fff", letterSpacing: "-0.05em",
          textShadow: "0 1px 0 rgba(0,0,0,0.35)",
          mixBlendMode: "screen",
        }}>{glyph}</div>
      )}
    </div>
  );
}

window.CrystalIcon = CrystalIcon;
window.CRYSTAL_PALETTES = CRYSTAL_PALETTES;

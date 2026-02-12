use crate::types::{LatencyEvent, Metrics, Pool, Position};
use maud::{html, Markup, PreEscaped, DOCTYPE};

/// Cyberpunk / Hacker Aesthetic Dashboard
pub fn dashboard_view(metrics: &Metrics, positions: &[Position]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "AETHER SNIPER // GOD MODE" }

                // Fonts
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin="";
                link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;700&family=Orbitron:wght@400;700;900&display=swap" rel="stylesheet";

                // HTMX & SSE
                script src="https://unpkg.com/htmx.org@1.9.10" {}
                script src="https://unpkg.com/htmx.org@1.9.10/dist/ext/sse.js" {}

                style {
                    (PreEscaped(r#"
                        :root {
                            --bg-color: #0f021a; /* Deep Rabbit Hole Purple */
                            --term-green: #00fff9; /* Alice Blue/Cyan */
                            --term-dim: #b829ff; /* Cheshire Cat Purple */
                            --term-red: #ff003c; /* Queen of Hearts Red */
                            --term-blue: #ffffff; /* White Rabbit */
                            --term-yellow: #ffb700; /* Mad Hatter Gold */
                            --glass: rgba(20, 5, 30, 0.65); /* Dreamy Glass */
                            --border: 1px solid #b829ff;
                        }
                        body {
                            background-color: var(--bg-color);
                            color: var(--term-green);
                            font-family: 'JetBrains Mono', monospace;
                            margin: 0;
                            height: 100vh;
                            overflow: hidden;
                            display: flex;
                            flex-direction: column;
                            text-shadow: 0 0 4px rgba(0, 255, 249, 0.3); /* Soft Bloom (뽀샤시) */
                        }
                        body::before {
                            content: " ";
                            display: block;
                            position: absolute;
                            top: 0; left: 0; bottom: 0; right: 0;
                            background: radial-gradient(circle at 50% 50%, rgba(184, 41, 255, 0.1), transparent 70%);
                            z-index: 0;
                            pointer-events: none;
                        }
                        header {
                            padding: 1rem 2rem;
                            border-bottom: 2px solid var(--term-dim);
                            display: flex;
                            justify-content: space-between;
                            align-items: flex-end;
                            background: rgba(10, 0, 20, 0.9);
                            z-index: 10;
                            box-shadow: 0 0 15px var(--term-dim);
                        }
                        h1 {
                            font-family: 'Orbitron', sans-serif;
                            font-weight: 900;
                            font-size: 2.2rem;
                            margin: 0;
                            text-shadow: 0 0 10px var(--term-dim);
                            letter-spacing: 2px;
                            color: var(--term-blue);
                        }
                        .status-badge {
                            border: 1px solid var(--term-green);
                            padding: 0.3rem 1rem;
                            font-size: 0.8rem;
                            font-weight: bold;
                            text-transform: uppercase;
                            background: rgba(0, 255, 249, 0.1);
                            box-shadow: 0 0 10px var(--term-green);
                            color: var(--term-blue);
                            border-radius: 15px; /* Softer edges */
                            animation: float 3s ease-in-out infinite;
                        }
                        @keyframes float {
                            0% { transform: translateY(0px); box-shadow: 0 0 10px var(--term-green); }
                            50% { transform: translateY(-3px); box-shadow: 0 0 20px var(--term-green); }
                            100% { transform: translateY(0px); box-shadow: 0 0 10px var(--term-green); }
                        }
                        main {
                            flex: 1;
                            display: grid;
                            grid-template-columns: 3fr 2fr;
                            grid-template-rows: auto 1fr;
                            gap: 1.5rem;
                            padding: 1.5rem;
                            overflow: hidden;
                            z-index: 1;
                        }
                        .panel {
                            border: 1px solid var(--term-dim);
                            background: var(--glass);
                            padding: 1rem;
                            position: relative;
                            border-radius: 8px; /* Soft corners */
                            box-shadow: 0 0 15px rgba(184, 41, 255, 0.1);
                        }
                        .panel h2 {
                            font-family: 'Orbitron', sans-serif;
                            font-size: 1rem;
                            margin-top: 0;
                            border-bottom: 1px solid var(--term-dim);
                            padding-bottom: 0.5rem;
                            color: var(--term-yellow); /* Gold Headers */
                            text-transform: uppercase;
                            text-shadow: 0 0 5px var(--term-yellow);
                        }
                        .stats-grid {
                            grid-column: 1 / -1;
                            display: grid;
                            grid-template-columns: repeat(4, 1fr);
                            gap: 1.5rem;
                        }
                        .stat-box {
                            text-align: center;
                            padding: 1rem;
                            border: 1px solid var(--term-dim);
                            background: rgba(15, 2, 26, 0.6);
                            border-radius: 8px;
                            transition: transform 0.2s;
                        }
                        .stat-box:hover {
                            transform: scale(1.02);
                            box-shadow: 0 0 15px var(--term-dim);
                        }
                        .stat-label {
                            display: block;
                            color: var(--term-green);
                            font-size: 0.7rem;
                            text-transform: uppercase;
                            letter-spacing: 2px;
                            margin-bottom: 0.5rem;
                            opacity: 0.8;
                        }
                        .stat-value {
                            display: block;
                            font-size: 2rem;
                            font-weight: bold;
                            color: var(--term-blue);
                            text-shadow: 0 0 8px var(--term-blue);
                        }
                        .val-up { color: var(--term-green); text-shadow: 0 0 10px var(--term-green); }
                        .val-down { color: var(--term-red); text-shadow: 0 0 10px var(--term-red); }
                        
                        /* Latency Color Codes - Pastel/Neon */
                        .latency-good { color: #fff; text-shadow: 0 0 5px #fff; } /* White Rabbit (Fast) */
                        .latency-warn { color: var(--term-yellow); } /* Tea Party */
                        .latency-bad { color: var(--term-red); font-weight: bold; } /* Off with their heads! */
                        
                        .log-panel {
                            grid-column: 2;
                            grid-row: 2;
                            display: flex;
                            flex-direction: column;
                        }
                        #log-list {
                            list-style: none;
                            padding: 0;
                            margin: 0;
                            flex: 1;
                            overflow-y: auto;
                            font-size: 0.8rem;
                            scrollbar-width: thin;
                            scrollbar-color: var(--term-dim) var(--bg-color);
                            font-family: 'JetBrains Mono', monospace;
                        }
                        #log-list li {
                            padding: 0.4rem 0;
                            border-bottom: 1px dashed rgba(184, 41, 255, 0.3);
                            display: flex;
                            gap: 0.5rem;
                            align-items: center;
                        }
                        .timestamp { color: #888; font-size: 0.75rem; }
                        .mint { color: var(--term-yellow); }
                        
                        .positions-panel {
                            grid-column: 1;
                            grid-row: 2;
                            overflow: hidden;
                            display: flex;
                            flex-direction: column;
                        }
                        .table-container {
                            overflow-y: auto;
                        }
                        table {
                            width: 100%;
                            border-collapse: collapse;
                            font-size: 0.9rem;
                        }
                        th {
                            text-align: left;
                            color: var(--term-dim);
                            border-bottom: 2px solid var(--term-dim);
                            padding: 0.8rem;
                            font-weight: bold;
                        }
                        td {
                            padding: 0.6rem 0.5rem;
                            border-bottom: 1px dashed rgba(184, 41, 255, 0.3);
                        }
                    "#))
                }
            }
            body {
                header {
                    div {
                        h1 { "WONDERLAND" span style="color:var(--term-dim); font-size:1.2rem; vertical-align:middle; margin-left:10px" { " // PROTOCOL" } }
                        div style="font-size: 0.8rem; color: var(--term-green); margin-top: 5px" { "FOLLOW THE WHITE RABBIT... 🐇" }
                    }
                    div class="status-badge" { "DRINK ME" }
                }

                main {
                    div class="stats-grid" id="stats-update-container"
                         hx-ext="sse" sse-connect="/events" hx-trigger="sse:stats" hx-get="/api/stats" hx-swap="innerHTML" {
                        (stats_component(metrics))
                    }
                    div class="panel positions-panel" {
                        h2 { "ACTIVE OPERATIONS" }
                        div class="table-container" id="positions-update-container"
                             hx-ext="sse" sse-connect="/events" hx-trigger="sse:positions" hx-get="/api/positions" hx-swap="innerHTML" {
                            (positions_component(positions))
                        }
                    }
                    div class="panel log-panel" {
                        h2 { "SYSTEM LOG STREAM" }
                        div style="flex:1; overflow-y:auto; display:flex; flex-direction:column-reverse" {
                            ul id="log-list"
                                hx-ext="sse" sse-connect="/events" sse-swap="log" hx-swap="afterbegin" {
                                li { "> [SYSTEM] AETHER DASHBOARD INITIALIZED... WAITING FOR SIGNAL" }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn stats_component(metrics: &Metrics) -> Markup {
    html! {
        div class="stat-box" {
            span class="stat-label" { "WALLET BALANCE" }
            span class="stat-value" { (format!("{:.4} SOL", metrics.wallet_balance_sol)) }
        }
        div class="stat-box" {
            span class="stat-label" { "TOTAL PNL" }
            span class=(format!("stat-value {}", if metrics.total_pnl_sol >= 0.0 { "val-up" } else { "val-down" })) {
                (format!("{:.4} SOL", metrics.total_pnl_sol))
            }
        }
        div class="stat-box" {
            span class="stat-label" { "WIN RATE" }
            span class="stat-value" {
                @if metrics.total_trades > 0 {
                    (format!("{}/{} ({:.1}%)", metrics.winning_trades, metrics.total_trades,
                        (metrics.winning_trades as f64 / metrics.total_trades as f64) * 100.0))
                } @else {
                    "N/A (0)"
                }
            }
        }
        div class="stat-box" {
            span class="stat-label" { "SCANNED POOLS" }
            span class="stat-value" { (metrics.pools_detected) }
        }
    }
}

pub fn positions_component(positions: &[Position]) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "TARGET (MINT)" }
                    th { "ENTRY" }
                    th { "ROI %" }
                    th { "STATUS" }
                }
            }
            tbody {
                @if positions.is_empty() {
                    tr {
                        td colspan="4" style="text-align: center; color: var(--term-dim); padding: 2rem; font-style: italic; opacity: 0.7" {
                            "// NO TEA PARTY YET... WAITING FOR GUESTS ☕🎩"
                        }
                    }
                } @else {
                    @for position in positions {
                        (position_item(position))
                    }
                }
            }
        }
    }
}

pub fn position_item(position: &Position) -> Markup {
    let pnl = position.unrealized_pnl_percent;
    let pnl_class = if pnl >= 0.0 { "val-up" } else { "val-down" };
    html! {
        tr {
            td {
                strong class="mint" {
                    (position.pool.base_mint.to_string()[..8])
                    "..."
                }
            }
            td { (format!("{:.6}", position.entry_price_sol)) }
            td class=(pnl_class) { (format!("{:.2}%", pnl)) }
            td { (format!("{:?}", position.status)) }
        }
    }
}

pub fn pool_list_item(pool: &Pool, latency: Option<&LatencyEvent>, lat_class: &str) -> Markup {
    let lat_str = if let Some(l) = latency {
        if l.pool_parsed_us > 1_600_000_000_000_000 && l.ws_receive_us > 1_600_000_000_000_000 {
            let diff = l.pool_parsed_us.saturating_sub(l.ws_receive_us);
            format!("{}us", diff)
        } else {
            format!("{}us", l.pool_parsed_us)
        }
    } else {
        "N/A".to_string()
    };
    html! {
        li {
            span class="timestamp" { (pool.detected_at.format("%H:%M:%S.%3f")) }
            " [DETECTED] "
            span class="mint" { (pool.base_mint.to_string()[..8]) "..." }
            " (Slot: " (pool.slot) ")"
            " "
            span class=(lat_class) { "[" (lat_str) "]" }
        }
    }
}

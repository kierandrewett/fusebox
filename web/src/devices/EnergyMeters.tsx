import type { EnergyView } from "../types";
const fmt = new Intl.NumberFormat("en-GB", { maximumFractionDigits: 2 });
export function EnergyMeters({ energy }: { energy?: EnergyView | null }) {
  if (!energy) return <p className="muted">No energy data yet.</p>;
  return <div className="meters"><span>{fmt.format((energy.current_power_mw ?? 0) / 1000)} W now</span><span>{fmt.format(energy.today_energy_wh / 1000)} kWh today</span><span>£{fmt.format(energy.today_cost_pence / 100)}</span></div>;
}

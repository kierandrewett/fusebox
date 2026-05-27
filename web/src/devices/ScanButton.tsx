export function ScanButton({ scanning, onScan }: { scanning: boolean; onScan: () => void }) {
  return <button type="button" className="scan-button" disabled={scanning} onClick={onScan}>{scanning ? "Scanning..." : "Scan for devices"}</button>;
}

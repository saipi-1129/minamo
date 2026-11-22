import IOKit.ps

/// Reads whether the Mac is currently running from the power adapter.
public enum PowerSource {
    public static func isOnACPower() -> Bool {
        guard let snapshot = IOPSCopyPowerSourcesInfo()?.takeRetainedValue(),
              let type = IOPSGetProvidingPowerSourceType(snapshot)?
                .takeUnretainedValue() as String?
        else {
            return false
        }
        return type == kIOPSACPowerValue
    }
}

/** Pause-diagnostic durations are monotonic, not wall clock.
 *
 *  BridgeSession.notePark records both clocks; elapsedSincePrevPark
 *  measures against nanos with a wall fallback for records predating
 *  the field. A backward wall step (NTP) between parks must not leak
 *  into elapsedSincePreviousStopMs.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/PauseMonoJavaCheck.java
 *  Run from the repo root: java -cp <bridge classes>:<out> PauseMonoJavaCheck
 */
public class PauseMonoJavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    public static void main(String[] args) {
        // Monotonic path: -1h wall step between parks leaves the 50ms
        // real elapsed intact.
        check(BridgeSession.elapsedSincePrevPark(
                1_000_000_000L, 1_000_000L,
                1_000_000L - 3_600_000L, 1_050_000_000L) == 50L,
                "mono immune to backward wall step");
        // Monotonic clamp: never negative, even if nanos went backward.
        check(BridgeSession.elapsedSincePrevPark(
                2_000_000_000L, 1_000_000L,
                1_000_000L, 1_000_000_000L) == 0L,
                "mono clamps at zero");
        // Wall fallback preserved for records predating the nanos field.
        check(BridgeSession.elapsedSincePrevPark(
                0L, 1_000L, 4_600L, 9_999_000_000L) == 3_600L,
                "wall fallback for pre-nanos records");
        if (failures > 0) {
            System.exit(1);
        }
        System.out.println("ok: PauseMonoJavaCheck all green");
    }
}

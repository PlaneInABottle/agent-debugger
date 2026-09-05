"""Long runner for idle-pump (continue-timeout) tests."""
import time

for i in range(30):
    time.sleep(1)  # <-- breakpoint here (cond i == 2)
print("AWAKE")

"""TEMPORARY (removed before merge): what does Ctrl+Break to a process group
id reach on Windows? Run inside its own console (see stress.yml) so a
console-wide event cannot touch the CI job."""
import ctypes, subprocess, sys, time

CTRL_BREAK_EVENT = 1
k32 = ctypes.WinDLL('kernel32', use_last_error=True)
HandlerRoutine = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_uint)
ignore = HandlerRoutine(lambda event: True)  # this process survives breaks
k32.SetConsoleCtrlHandler(ignore, True)
out = open(sys.argv[1], 'a')

def bystander():
    # Same console and group as this process: only a console-wide (or
    # this-group) event reaches it.
    return subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])

def case(name, target_of):
    b = bystander()
    time.sleep(1)
    target = target_of()
    ok = k32.GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, target)
    time.sleep(2)
    out.write(f'{name}: send={ok} bystander_died={b.poll() is not None}\n')
    out.flush()
    b.kill()

def live_leader():
    p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'],
                         creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    time.sleep(1)
    return p.pid

def exited_leader():
    p = subprocess.Popen([sys.executable, '-c', 'pass'],
                         creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    p.wait()  # handle stays open: the id is not reused
    return p.pid

def live_non_leader():
    p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
    time.sleep(1)
    return p.pid

case('live group leader', live_leader)
case('exited group leader', exited_leader)
case('live process that leads no group', live_non_leader)
case('group 0', lambda: 0)

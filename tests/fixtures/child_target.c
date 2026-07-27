#include <stdio.h>
#include <windows.h>

/* Fixture for multi-process debugging tests (see docs/04_テスト仕様書.md).
   Spawns a child process (cmd.exe) which in turn spawns its own child
   (ping.exe), giving a root -> child -> grandchild process tree for
   tdb-windows's DEBUG_PROCESS-based tracking to exercise. */
int main(void)
{
    STARTUPINFOA si;
    PROCESS_INFORMATION pi;
    ZeroMemory(&si, sizeof(si));
    si.cb = sizeof(si);
    ZeroMemory(&pi, sizeof(pi));

    char cmdline[] = "cmd.exe /c \"ping -n 2 127.0.0.1 >nul\"";

    printf("parent: launching child\n");
    if (!CreateProcessA(NULL, cmdline, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        printf("parent: CreateProcess failed: %lu\n", GetLastError());
        return 1;
    }

    printf("parent: child pid = %lu\n", pi.dwProcessId);
    WaitForSingleObject(pi.hProcess, INFINITE);
    printf("parent: child exited\n");

    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);

    printf("parent: done\n");
    return 0;
}

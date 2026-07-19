#include <stdio.h>
#include <windows.h>

/* Shared counter incremented by every worker under a lock: a good `watch`
   target for confirming a hardware watchpoint fires no matter which
   thread performs the write (debug registers are synced to every
   thread, not just the one that was running when the watchpoint was set). */
int g_shared_counter = 0;

CRITICAL_SECTION g_lock;

DWORD WINAPI worker(LPVOID arg)
{
    int id = (int)(intptr_t)arg;

    for (int i = 0; i < 5; i++)
    {
        EnterCriticalSection(&g_lock);
        g_shared_counter++;
        printf("worker %d: g_shared_counter = %d\n", id, g_shared_counter);
        LeaveCriticalSection(&g_lock);
        Sleep(50);
    }

    return 0;
}

int main(void)
{
    InitializeCriticalSection(&g_lock);

    HANDLE threads[3];
    for (int i = 0; i < 3; i++)
    {
        threads[i] = CreateThread(NULL, 0, worker, (LPVOID)(intptr_t)(i + 1), 0, NULL);
    }

    /* By the time this prints, all three worker threads exist: a good spot
       for a breakpoint before running `threads` to see all four (main +
       3 workers) listed at once. */
    printf("main: 3 worker threads started\n");

    WaitForMultipleObjects(3, threads, TRUE, INFINITE);

    for (int i = 0; i < 3; i++)
    {
        CloseHandle(threads[i]);
    }

    printf("main: all workers done, g_shared_counter = %d\n", g_shared_counter);

    DeleteCriticalSection(&g_lock);
    return 0;
}

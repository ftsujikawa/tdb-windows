#include <stdio.h>
#include <windows.h>

/* Fixture for thread-management and hardware-watchpoint tests (see
   docs/04_テスト仕様書.md). Three worker threads repeatedly write to a
   shared global under a lock, so a `watch` on g_shared_counter reliably
   fires regardless of which thread performs the write, and `threads` has
   more than just the main thread to list while workers are alive. */

int g_shared_counter = 0;
CRITICAL_SECTION g_lock;

DWORD WINAPI worker(LPVOID arg)
{
    int id = (int)(intptr_t)arg;
    for (int i = 0; i < 20; i++)
    {
        EnterCriticalSection(&g_lock);
        g_shared_counter++;
        printf("worker %d: g_shared_counter = %d\n", id, g_shared_counter);
        LeaveCriticalSection(&g_lock);
        Sleep(30);
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

    printf("main: %d worker threads started\n", 3);

    WaitForMultipleObjects(3, threads, TRUE, INFINITE);
    for (int i = 0; i < 3; i++)
    {
        CloseHandle(threads[i]);
    }

    printf("main: all workers done, g_shared_counter = %d\n", g_shared_counter);
    DeleteCriticalSection(&g_lock);
    return 0;
}

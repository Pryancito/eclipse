#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <poll.h>
#include <assert.h>
#include <time.h>
#include <string.h>
#include <signal.h>

static void hammer_short_waits(void)
{
    struct timespec req = { .tv_sec = 0, .tv_nsec = 1000000 };
    for (int i = 0; i < 200; i++) {
        assert(poll(NULL, 0, 1) == 0);
        assert(nanosleep(&req, NULL) == 0);
    }
}

static void test_wait_survives_short_deadline_hammer(void)
{
    int pipefd[2];
    assert(pipe(pipefd) == 0);

    pid_t waiter = fork();
    assert(waiter >= 0);
    if (waiter == 0) {
        int ep = epoll_create1(0);
        struct epoll_event ev, out;
        char c;
        signal(SIGCHLD, SIG_IGN);
        assert(ep >= 0);
        memset(&ev, 0, sizeof(ev));
        ev.events = EPOLLIN;
        ev.data.fd = pipefd[0];
        assert(epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &ev) == 0);
        /* Leave behind an ignored SIGCHLD while a neighbouring process hammers
         * poll(0,1ms)/nanosleep(1ms): epoll_wait must stay blocked until the
         * pipe becomes readable, never fail or return spuriously. */
        pid_t helper = fork();
        assert(helper >= 0);
        if (helper == 0)
            _exit(0);
        assert(epoll_wait(ep, &out, 1, 5000) == 1);
        assert(out.data.fd == pipefd[0]);
        assert((out.events & EPOLLIN) != 0);
        assert(read(pipefd[0], &c, 1) == 1);
        assert(c == '!');
        _exit(0);
    }

    pid_t hammer = fork();
    assert(hammer >= 0);
    if (hammer == 0) {
        hammer_short_waits();
        _exit(0);
    }

    int status;
    assert(waitpid(hammer, &status, 0) == hammer);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    assert(waitpid(waiter, &status, WNOHANG) == 0);
    assert(write(pipefd[1], "!", 1) == 1);
    assert(waitpid(waiter, &status, 0) == waiter);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(pipefd[0]);
    close(pipefd[1]);
}

int main(void)
{
    int ret;
    struct pollfd fds[2];
    int pipefd[2];
    struct timespec ts;

    // test poll using pipe
    if (pipe(pipefd) == -1)
    {
        printf("pipe");
        exit(-1);
    }

    // test time out
    fds[0].fd = 0;
    fds[0].events = POLLIN;
    ret = poll(fds, 1, 1000);
    assert(ret == 0);

    fds[0].fd = pipefd[0];
    fds[0].events = POLLIN;
    fds[1].fd = pipefd[1];
    fds[1].events = POLLOUT;

    ret = poll(fds, 2, 5000);
    assert(ret == 1);
    assert(fds[1].revents == POLLOUT);

    write(pipefd[1], "test", strlen("test"));

    ts.tv_sec = 5;
    ts.tv_nsec = 0;

    ret = ppoll(fds, 2, &ts, NULL);
    assert(ret == 2);
    assert(fds[0].revents == POLLIN);

    close(pipefd[0]);
    close(pipefd[1]);
    test_wait_survives_short_deadline_hammer();
    return 0;
}
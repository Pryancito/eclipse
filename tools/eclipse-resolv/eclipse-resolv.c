/*
 * Static DNS lookup helper for Eclipse OS (syscall 601).
 * Usage: eclipse-resolv [-4|-6] hostname
 */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define SYS_eclipse_dns_query 601

struct dns_result_entry {
	uint16_t family;
	uint16_t _pad;
	uint8_t addr[16];
};

static void usage(const char *argv0)
{
	fprintf(stderr, "usage: %s [-4|-6] hostname\n", argv0);
	exit(2);
}

int main(int argc, char **argv)
{
	struct dns_result_entry entries[16];
	char buf[INET6_ADDRSTRLEN];
	int family = AF_UNSPEC;
	int i, n;
	const char *name;

	if (argc < 2)
		usage(argv[0]);

	if (strcmp(argv[1], "-4") == 0) {
		family = AF_INET;
		name = argv[2];
		if (!name)
			usage(argv[0]);
	} else if (strcmp(argv[1], "-6") == 0) {
		family = AF_INET6;
		name = argv[2];
		if (!name)
			usage(argv[0]);
	} else {
		name = argv[1];
	}

	n = (int)syscall(SYS_eclipse_dns_query, name, strlen(name), family, entries, 16);
	if (n < 0) {
		perror("eclipse_dns_query");
		return 1;
	}
	if (n == 0) {
		fprintf(stderr, "%s: not found\n", name);
		return 1;
	}

	for (i = 0; i < n; i++) {
		if (entries[i].family == AF_INET) {
			struct in_addr a;
			memcpy(&a, entries[i].addr, 4);
			inet_ntop(AF_INET, &a, buf, sizeof(buf));
			printf("%s\n", buf);
		} else if (entries[i].family == AF_INET6) {
			struct in6_addr a6;
			memcpy(&a6, entries[i].addr, 16);
			inet_ntop(AF_INET6, &a6, buf, sizeof(buf));
			printf("%s\n", buf);
		}
	}
	return 0;
}

/*
 * Eclipse OS DNS resolver shim.
 *
 * - Shared lib (libeclipse_dns.so): LD_PRELOAD overrides musl getaddrinfo.
 * - Static wrap (resolv_wrap_*.o): link busybox with -Wl,--wrap=getaddrinfo.
 *
 * Uses kernel syscall __NR_eclipse_dns_query (601).
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

#define SYS_eclipse_dns_query 601

#ifdef ECLIPSE_RESOLV_WRAP
#define GA_NAME __wrap_getaddrinfo
#define GA_FREE __wrap_freeaddrinfo
#define GA_STRERROR __wrap_gai_strerror
#else
#define GA_NAME getaddrinfo
#define GA_FREE freeaddrinfo
#define GA_STRERROR gai_strerror
#endif

struct dns_result_entry {
	uint16_t family;
	uint16_t _pad;
	uint8_t addr[16];
};

static long eclipse_dns_query(const char *name, int family,
	struct dns_result_entry *out, size_t out_max)
{
	return syscall(SYS_eclipse_dns_query, name, strlen(name), family, out, out_max);
}

static int try_numeric(const char *node, const char *service,
	const struct addrinfo *hints, struct addrinfo **res)
{
	struct addrinfo *ai;
	struct sockaddr_in sin;
	struct sockaddr_in6 sin6;
	int port = 0;

	if (service && *service) {
		port = atoi(service);
		if (port < 0 || port > 65535)
			return EAI_SERVICE;
	}

	if (strchr(node, ':')) {
		if (hints && hints->ai_family == AF_INET)
			return EAI_NONAME;
		memset(&sin6, 0, sizeof(sin6));
		sin6.sin6_family = AF_INET6;
		sin6.sin6_port = htons((uint16_t)port);
		if (inet_pton(AF_INET6, node, &sin6.sin6_addr) != 1)
			return EAI_NONAME;
		ai = calloc(1, sizeof(*ai));
		if (!ai) return EAI_MEMORY;
		ai->ai_family = AF_INET6;
		ai->ai_socktype = hints ? hints->ai_socktype : SOCK_STREAM;
		ai->ai_protocol = hints ? hints->ai_protocol : 0;
		ai->ai_addrlen = sizeof(sin6);
		ai->ai_addr = malloc(sizeof(sin6));
		if (!ai->ai_addr) { free(ai); return EAI_MEMORY; }
		memcpy(ai->ai_addr, &sin6, sizeof(sin6));
		*res = ai;
		return 0;
	}

	if (hints && hints->ai_family == AF_INET6)
		return EAI_NONAME;
	memset(&sin, 0, sizeof(sin));
	sin.sin_family = AF_INET;
	sin.sin_port = htons((uint16_t)port);
	if (inet_pton(AF_INET, node, &sin.sin_addr) != 1)
		return EAI_NONAME;
	ai = calloc(1, sizeof(*ai));
	if (!ai) return EAI_MEMORY;
	ai->ai_family = AF_INET;
	ai->ai_socktype = hints ? hints->ai_socktype : SOCK_STREAM;
	ai->ai_protocol = hints ? hints->ai_protocol : 0;
	ai->ai_addrlen = sizeof(sin);
	ai->ai_addr = malloc(sizeof(sin));
	if (!ai->ai_addr) { free(ai); return EAI_MEMORY; }
	memcpy(ai->ai_addr, &sin, sizeof(sin));
	*res = ai;
	return 0;
}

int GA_NAME(const char *node, const char *service,
	const struct addrinfo *hints, struct addrinfo **res)
{
	struct dns_result_entry entries[16];
	struct addrinfo *head = NULL, *tail = NULL;
	int family, port, i, n, err;

	if (!node || !res)
		return EAI_NONAME;
	*res = NULL;

	if (strchr(node, ':') || strchr(node, '.')) {
		err = try_numeric(node, service, hints, res);
		if (err == 0)
			return 0;
		if (err != EAI_NONAME)
		 return err;
	}

	family = hints ? hints->ai_family : AF_UNSPEC;
	n = (int)eclipse_dns_query(node, family, entries, 16);
	if (n < 0) {
		if (errno == ENOENT || errno == ETIMEDOUT)
			return EAI_NONAME;
		return EAI_SYSTEM;
	}
	if (n == 0)
		return EAI_NONAME;

	port = 0;
	if (service && *service) {
		char *end = NULL;
		long p = strtol(service, &end, 10);
		if (end && *end == '\0' && p >= 0 && p <= 65535) {
			port = (int)p;
		} else {
			/* Common service names; getservbyname may be unavailable. */
			if (!strcmp(service, "http")) port = 80;
			else if (!strcmp(service, "https")) port = 443;
			else if (!strcmp(service, "ftp")) port = 21;
			else if (!strcmp(service, "ssh")) port = 22;
			else if (!strcmp(service, "domain") || !strcmp(service, "dns")) port = 53;
			else if (!strcmp(service, "ntp")) port = 123;
			else return EAI_SERVICE;
		}
	}

	for (i = 0; i < n; i++) {
		struct addrinfo *ai = calloc(1, sizeof(*ai));
		if (!ai) { freeaddrinfo(head); return EAI_MEMORY; }

		if (entries[i].family == AF_INET) {
			struct sockaddr_in *sin = malloc(sizeof(*sin));
			if (!sin) { free(ai); freeaddrinfo(head); return EAI_MEMORY; }
			memset(sin, 0, sizeof(*sin));
			sin->sin_family = AF_INET;
			sin->sin_port = htons((uint16_t)port);
			memcpy(&sin->sin_addr, entries[i].addr, 4);
			ai->ai_family = AF_INET;
			ai->ai_addrlen = sizeof(*sin);
			ai->ai_addr = (struct sockaddr *)sin;
		} else if (entries[i].family == AF_INET6) {
			struct sockaddr_in6 *sin6 = malloc(sizeof(*sin6));
			if (!sin6) { free(ai); freeaddrinfo(head); return EAI_MEMORY; }
			memset(sin6, 0, sizeof(*sin6));
			sin6->sin6_family = AF_INET6;
			sin6->sin6_port = htons((uint16_t)port);
			memcpy(&sin6->sin6_addr, entries[i].addr, 16);
			ai->ai_family = AF_INET6;
			ai->ai_addrlen = sizeof(*sin6);
			ai->ai_addr = (struct sockaddr *)sin6;
		} else {
			free(ai);
			continue;
		}

		ai->ai_socktype = hints ? hints->ai_socktype : SOCK_STREAM;
		ai->ai_protocol = hints ? hints->ai_protocol : 0;
		ai->ai_next = NULL;
		if (!head) head = ai;
		else tail->ai_next = ai;
		tail = ai;
	}

	if (!head)
		return EAI_NONAME;
	*res = head;
	return 0;
}

void GA_FREE(struct addrinfo *res)
{
	while (res) {
		struct addrinfo *next = res->ai_next;
		free(res->ai_addr);
		free(res->ai_canonname);
		free(res);
		res = next;
	}
}

const char *GA_STRERROR(int errcode)
{
	switch (errcode) {
	case EAI_AGAIN: return "Temporary failure in name resolution";
	case EAI_BADFLAGS: return "Invalid flags";
	case EAI_FAIL: return "Non-recoverable failure in name resolution";
	case EAI_FAMILY: return "Address family not supported";
	case EAI_MEMORY: return "Memory allocation failure";
	case EAI_NONAME: return "Name does not resolve";
	case EAI_SERVICE: return "Service not supported";
	case EAI_SOCKTYPE: return "Socket type not supported";
	case EAI_SYSTEM: return "System error";
	default: return "Unknown error";
	}
}

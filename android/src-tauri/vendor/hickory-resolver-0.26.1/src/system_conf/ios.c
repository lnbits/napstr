// Keep Darwin's opaque resolver layout in C, using the public iOS SDK headers.
#include <arpa/inet.h>
#include <netinet/in.h>
#include <resolv.h>
#include <stdint.h>
#include <string.h>

struct NapstrfyDnsServer {
    uint8_t address[16];
    uint32_t scope_id;
    uint16_t port;
    uint8_t family;
};

struct NapstrfyDnsConfig {
    struct NapstrfyDnsServer servers[3];
    uint32_t count;
    uint32_t ndots;
    char search_domains[6][256];
};

_Static_assert(sizeof(struct NapstrfyDnsServer) == 24, "DNS server FFI layout changed");
_Static_assert(sizeof(struct NapstrfyDnsConfig) == 1616, "DNS config FFI layout changed");

int napstrfy_ios_read_dns(struct NapstrfyDnsConfig *out) {
    struct __res_state state = {0};
    memset(out, 0, sizeof(*out));
    if (res_ninit(&state) != 0) return -1;

    union res_sockaddr_union servers[3] = {0};
    int count = res_getservers(&state, servers, 3);
    for (int i = 0; i < count && i < 3; ++i) {
        struct NapstrfyDnsServer *server = &out->servers[out->count];
        if (servers[i].sin.sin_family == AF_INET) {
            server->family = 4;
            memcpy(server->address, &servers[i].sin.sin_addr, 4);
            server->port = ntohs(servers[i].sin.sin_port);
        } else if (servers[i].sin6.sin6_family == AF_INET6) {
            server->family = 6;
            memcpy(server->address, &servers[i].sin6.sin6_addr, 16);
            server->scope_id = servers[i].sin6.sin6_scope_id;
            server->port = ntohs(servers[i].sin6.sin6_port);
        } else {
            continue;
        }
        ++out->count;
    }
    out->ndots = state.ndots;
    for (int i = 0; i < MAXDNSRCH && i < 6 && state.dnsrch[i]; ++i) {
        strlcpy(out->search_domains[i], state.dnsrch[i], sizeof(out->search_domains[i]));
    }
    res_ndestroy(&state);
    return 0;
}

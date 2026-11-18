// Exercise the iOS resolver bridge with deterministic IPv4/IPv6 SDK structures.
#include <assert.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <resolv.h>
#include <string.h>

static int fail_init;
static int destroyed;
static int fake_init(res_state state);
static int fake_servers(res_state state, union res_sockaddr_union *servers, int count);
static void fake_destroy(res_state state);
#undef res_ninit
#undef res_getservers
#undef res_ndestroy
#define res_ninit fake_init
#define res_getservers fake_servers
#define res_ndestroy fake_destroy
#include "../src-tauri/vendor/hickory-resolver-0.26.1/src/system_conf/ios.c"

static int fake_init(res_state state) {
    if (fail_init) return -1;
    state->ndots = 2;
    state->dnsrch[0] = "example.test";
    return 0;
}

static int fake_servers(res_state state, union res_sockaddr_union *servers, int count) {
    (void)state;
    assert(count == 3);
    servers[0].sin.sin_family = AF_INET;
    servers[0].sin.sin_port = htons(5353);
    assert(inet_pton(AF_INET, "192.0.2.53", &servers[0].sin.sin_addr) == 1);
    servers[1].sin6.sin6_family = AF_INET6;
    servers[1].sin6.sin6_port = htons(53);
    servers[1].sin6.sin6_scope_id = 7;
    assert(inet_pton(AF_INET6, "fe80::53", &servers[1].sin6.sin6_addr) == 1);
    servers[2].sin.sin_family = AF_UNSPEC;
    return 4; // The bridge must respect buffer capacity, even with a larger total.
}

static void fake_destroy(res_state state) {
    (void)state;
    ++destroyed;
}

int main(void) {
    struct NapstrfyDnsConfig config;
    assert(napstrfy_ios_read_dns(&config) == 0);
    assert(config.count == 2 && destroyed == 1);
    assert(config.servers[0].family == 4 && config.servers[0].port == 5353);
    assert(memcmp(config.servers[0].address, (uint8_t[]){192, 0, 2, 53}, 4) == 0);
    assert(config.servers[1].family == 6 && config.servers[1].scope_id == 7);
    assert(config.servers[1].port == 53 && config.servers[1].address[15] == 0x53);
    assert(config.ndots == 2 && strcmp(config.search_domains[0], "example.test") == 0);
    assert(config.search_domains[1][0] == 0);
    fail_init = 1;
    assert(napstrfy_ios_read_dns(&config) == -1);
    assert(config.count == 0 && destroyed == 1);
    return 0;
}

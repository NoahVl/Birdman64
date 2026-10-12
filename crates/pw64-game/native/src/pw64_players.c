/*
 * Split-screen player context switch (splitscreen.md "M2"). The game keeps
 * one player's state in its globals (Unk80362690.unkC[0], vehicle
 * pointers, HUD, shadow, vehicle file statics, ...). With 2 players each
 * registered region has one saved copy per player; pw64_player_switch
 * saves the live bytes into the current player's slot and loads the other
 * player's. unk9C stays 0: unkC[0] itself is a registered region.
 * Fixed arena, no malloc (the game module has none of its own).
 */

int pw64_players = 1;
int pw64_level_players = 1;
int pw64_player_cur = 0;

#define PW64_MAX_REGIONS 192
#define PW64_SLOT_BYTES 0x14000 /* objective arrays (pw64_reg_task) ~29 KB */

typedef struct {
    unsigned char* addr;
    unsigned int size;
    unsigned int off; /* into each slot */
} Pw64Region;

static Pw64Region sRegions[PW64_MAX_REGIONS];
static int sRegionCount;
static unsigned int sSlotUsed;
static unsigned char sSlots[PW64_MAX_PLAYERS][PW64_SLOT_BYTES];

static void pw64_copy(unsigned char* dst, const unsigned char* src, unsigned int n) {
    unsigned int i;
    for (i = 0; i < n; i++) {
        dst[i] = src[i];
    }
}

void pw64_player_register(void* addr, unsigned int size) {
    Pw64Region* r;
    int i;

    for (i = 0; i < sRegionCount; i++) {
        if (sRegions[i].addr == addr) {
            return; /* already registered */
        }
    }
    if (sRegionCount >= PW64_MAX_REGIONS) {
        pw64_fatal("pw64_player_register: too many regions", (unsigned int)sRegionCount);
    }
    if (sSlotUsed + size > PW64_SLOT_BYTES) {
        pw64_fatal("pw64_player_register: slot arena full", sSlotUsed + size);
    }
    r = &sRegions[sRegionCount++];
    r->addr = addr;
    r->size = size;
    r->off = sSlotUsed;
    sSlotUsed = (sSlotUsed + size + 7u) & ~7u;
    for (i = 0; i < PW64_MAX_PLAYERS; i++) {
        pw64_copy(&sSlots[i][r->off], r->addr, r->size);
    }
}

void pw64_player_switch(int i) {
    int k;

    if (i == pw64_player_cur) {
        return;
    }
    if (i < 0 || i >= pw64_players) {
        pw64_fatal("pw64_player_switch: bad player", (unsigned int)i);
    }
    for (k = 0; k < sRegionCount; k++) {
        Pw64Region* r = &sRegions[k];
        pw64_copy(&sSlots[pw64_player_cur][r->off], r->addr, r->size);
        pw64_copy(r->addr, &sSlots[i][r->off], r->size);
    }
    pw64_player_cur = i;
}

/* Player i's copy of the registered region at addr (the live bytes when i
 * is current); NULL if addr isn't a registered region's start. */
void* pw64_player_peek(int i, void* addr) {
    int k;

    if (i == pw64_player_cur) {
        return addr;
    }
    if (i < 0 || i >= pw64_players) {
        return 0;
    }
    for (k = 0; k < sRegionCount; k++) {
        if (sRegions[k].addr == (unsigned char*)addr) {
            return &sSlots[i][sRegions[k].off];
        }
    }
    return 0;
}

void pw64_players_sync(void) {
    int i, k;

    for (i = 0; i < pw64_players; i++) {
        if (i == pw64_player_cur) {
            continue;
        }
        for (k = 0; k < sRegionCount; k++) {
            Pw64Region* r = &sRegions[k];
            pw64_copy(&sSlots[i][r->off], r->addr, r->size);
        }
    }
}

void pw64_players_init(void) {
    pw64_reg_game();
    pw64_reg_camera();
    pw64_reg_hud();
    pw64_reg_shadow();
    pw64_reg_hang_glider0();
    pw64_reg_hang_glider1();
    pw64_reg_hang_glider2();
    pw64_reg_rocket_belt();
    pw64_reg_rocket_belt_sound();
    pw64_reg_rocket_belt_poses();
    pw64_reg_rocket_belt_move();
    pw64_reg_gyrocopter();
    pw64_reg_gyrocopter_sound();
    pw64_reg_gyrocopter_poses();
    pw64_reg_gyrocopter_move();
    pw64_reg_jumble_hopper();
    pw64_reg_birdman();
    pw64_reg_birdman_sound();
    pw64_reg_birdman_flutter();
    pw64_reg_birdman_move();
    pw64_reg_cannonball();
    pw64_reg_cannonball_pilot();
    pw64_reg_skydiving();
    pw64_reg_skydiving_drift();
    pw64_reg_skydiving_sound();
    pw64_reg_task();
    pw64_reg_snap();
}

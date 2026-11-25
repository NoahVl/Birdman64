/*
 * Plane variant of the gyrocopter (experimental; Settings "Gyrocopter: Small
 * plane / Big plane", PW64_PLANE=small|big, PW64_START=<class>:ps|pl:<test>).
 * The fixed-wing flight
 * model is based on MegaMech's aeroplane.c and reworked here: drag that
 * bounds the top speed, induced drag, an angle-of-attack stall, takeoff
 * rotation, wheel brakes and attitude guards that keep clear of the gyro's
 * tumble test.
 *
 * The plane reuses the gyrocopter's slot and code: GyrocopterData, ground
 * contact, collisions, crash handling, fuel, missiles and HUD. Small hooks
 * in the patched gyrocopter files call in here; each returns 0 / does
 * nothing unless the current test flies the plane (pw64_plane_run), so the
 * stock gyrocopter path is unchanged:
 *   gyrocopterLoadLevel   -> pw64_plane_load    model, crash model, geometry
 *   gyrocopterEnterLeave  -> pw64_plane_enter   camera distances, model pose
 *   uvDobjPosm(obj, 0)    -> pw64_plane_posm    model origin offset
 *   func_8030399C         -> pw64_plane_forces  func_80308478/80307BAC/80307EA8
 *                         -> pw64_plane_control func_80304738 (airborne)
 *                         -> pw64_plane_accel   func_80309090 (airborne)
 *   func_802F50CC / func_802F5358 / func_802F56B4
 *                         -> pw64_plane_pose / _props / _rest (no rotor parts)
 *   test setup / leave    -> pw64_plane_end     drop the run's results
 *
 * Frames (gyrocopter.c): local x = right, y = forward, z = up. unk190 =
 * local velocity, unk364 = airflow (wind - velocity), unk31C = local
 * acceleration summed this frame (unk334: the part without gravity and
 * body drag, which the ground solver reads), unk1EC/1F0/1F4 = yaw, pitch
 * (+ = nose up), roll (+ = right wing down); unk1F8/1FC/200 their rates.
 *
 * Saves: a plane run never writes the save file (game.c.patch), and the
 * in-memory results it tallied are rolled back when the test is left, so a
 * later save can't pick them up either.
 */

#include "common.h"
#include <uv_dobj.h>
#include <uv_fx.h>
#include <uv_graphics.h>
#include <uv_math.h>
#include <uv_matrix.h>
#include <uv_memory.h>
#include "app/camera.h"
#include "app/code_66F70.h"
#include "app/code_7FE00.h"
#include "app/code_9A960.h"
#include "app/code_7CF30.h"
#include "app/environment.h"
#include "app/game.h"
#include "app/gyrocopter.h"
#include "app/hud.h"
#include "app/level.h"
#include "app/snd.h"
#include "app/task.h"
#include "app/text_data.h"
#include "app/demo.h"
#include "app/fdr.h"
#include "app/shadow.h"

/* gyrocopter.c globals (non-static there). */
extern u16 D_8036957E;                 /* the test's class */
extern Unk803599D0 D_803694D0;         /* fuel burn vs. throttle */
extern Unk802D3658_Unk111C D_80369070; /* collision capsules: [0] body, [1] rotor */

extern Unk803599D0 D_80368880;
extern s32 D_8034F380;
extern s32 D_8034F384;


/* Rust (pw64_game::set_plane, also live from the settings screen): which
 * aircraft the gyrocopter slot flies from the next test start:
 * PW64_PLANE_OFF (the gyrocopter), PW64_PLANE_BIG (model 39, or 27 where the
 * level didn't load 39) or PW64_PLANE_SMALL (model 27). */
int pw64_plane = PW64_PLANE_OFF;
/* 1 while the current test flies the plane: latched by the first
 * gyrocopterLoadLevel of a test (replays reload the vehicle and keep it),
 * cleared by pw64_plane_end. */
int pw64_plane_run = 0;

static s32 sFlapMode = 0;

/* ------------------------------------------------------------------------ */
/* Models                                                                    */
/* ------------------------------------------------------------------------ */

/* Per-model geometry in the vehicle frame (origin = centre of gravity).
 * Measured from the model meshes: both planes are one part with the nose
 * along +y; their origins are not the CG (the big plane's is behind its
 * tail), so `offset` moves the mesh into place when it is drawn. */
typedef struct {
    s16 modelId;
    Vec3F offset;             /* mesh origin in the vehicle frame */
    Vec3F gear[4];            /* unk5EC order: nose, left main, right main, tail */
    Vec3F bodyFront;          /* fuselage capsule (unk5BC .. unk5C8) */
    Vec3F bodyRear;
    f32 halfSpan, wingY, wingZ; /* wing capsule, replaces the rotor's */
    Vec3F chase;              /* chase camera point (unk628..630) */
    Vec3F eye;                /* cockpit camera (unk634..63C) */
    f32 camMin, camMax;       /* chase distance clamp (camera unk54/58) */
} PlaneModel;

/* MODEL_BIG_WHITE_RED_PLANE first (not in Ever-Frost's model list), then
 * MODEL_YELLOW_WHITE_PLANE (on every island): the big plane takes the first
 * one loaded, the small plane starts at index 1. */
static const PlaneModel sPlaneModels[] = {
    {
        MODEL_BIG_WHITE_RED_PLANE,
        { 0.0f, -19.8f, -3.0f },
        { { 0.0f, 6.8f, -2.5f }, { -2.6f, -1.1f, -2.5f }, { 2.6f, -1.1f, -2.5f }, { 0.0f, -16.5f, 0.6f } },
        { 0.0f, 9.5f, -1.0f },
        { 0.0f, -16.5f, 1.5f },
        13.6f, -1.2f, 1.9f,
        { 0.0f, -32.0f, 8.0f },
        { 0.0f, 5.5f, 0.8f },
        12.0f, 36.0f,
    },
    {
        MODEL_YELLOW_WHITE_PLANE,
        { 0.0f, -1.0f, -1.0f },
        { { 0.0f, 2.7f, -1.0f }, { -2.6f, -0.45f, -1.0f }, { 2.6f, -0.45f, -1.0f }, { 0.0f, -8.0f, 0.5f } },
        { 0.0f, 3.8f, 0.0f },
        { 0.0f, -8.0f, 1.0f },
        8.4f, -0.9f, 0.7f,
        { 0.0f, -16.0f, 4.5f },
        { 0.0f, 1.0f, 0.9f },
        7.0f, 20.0f,
    },
};

static const PlaneModel* sPlane;

/* The test's results before the plane run (pw64_plane_end restores them). */
static Unk80364210 sPlaneResults;

/* A model this level loaded, or NULL (uvDobjModel would keep the old one). */
static ParsedUVMD* plane_model_loaded(s32 modelId) {
    if ((modelId < 0) || (modelId >= LEVEL_MODEL_COUNT)) {
        return NULL;
    }
    return gGfxUnkPtrs->models[modelId];
}

/* ------------------------------------------------------------------------ */
/* Tuning (metres, seconds; accelerations in m/s^2)                         */
/* ------------------------------------------------------------------------ */

/* Estimated body drag coefficient of the gyro (a = AP_BODY_DRAG * V^2), fitted
 * to the old top-speed numbers (~45 m/s at 0.5 throttle, ~62 m/s at full).
 * If a mode's speed lands off target in game, tune this one value. */
#define AP_BODY_DRAG 0.0024f
#define AP_MIN_CD0 0.0005f
#define AP_FLAP_RATE 2.0f      /* eases toward the new mode (1/s) */
#define KMH_TO_MS(k) ((k) / 3.6f)

#define AP_GRAVITY 9.8f
/* Lift: K * CL * airspeed^2. CL = 0.35 (wing incidence alone) holds level
 * flight at ~45 m/s; CL_MAX gives a ~22 m/s stall. */
#define AP_CL_SLOPE 5.0f      /* CL per unit AoA (tan) */
#define AP_ALPHA_STALL 0.24f  /* stall AoA (tan, ~16 deg) */
#define AP_CL_MAX 1.4f
#define AP_CL_FALL 6.0f       /* CL lost per unit AoA past the stall */
#define AP_CL_POST_STALL 0.6f
#define AP_WING_INCIDENCE 0.07f
#define AP_GROUND_RAKE 0.03f  /* extra AoA from the gear stance */
#define AP_ROTATE_AOA 0.14f    /* full stick back on the runway (takeoff) */
/* Drag on top of the gyro's body drag (func_80307A9C, which levels off at
 * ~7.6 m/s^2): parasitic CD0 * V^2 and induced K_I * CL * lift. Top speed
 * ~62 m/s at full throttle, ~45 m/s at the 0.5 the throttle returns to. */
#define AP_INDUCED_K 0.04f
#define AP_THRUST_MAX 20.5f   /* static thrust at full throttle */
#define AP_PROP_VMAX 370.0f   /* thrust falls linearly to 0 at this airspeed */
#define AP_SIDE_DAMP 1.5f     /* fin: turns the velocity toward the nose */
#define AP_BRAKE 0.4f         /* wheel brakes, throttle closed on the ground */
#define AP_GROUND_SPOIL 0.3f  /* lift kept on the ground, throttle closed */
/* Control: attitude hold, the stick sets bank and pitch angles. */
#define AP_MAX_BANK 0.96f     /* 55 deg */
#define AP_MAX_PITCH 0.61f    /* 35 deg */
#define AP_ROLL_GAIN 0.8f     /* rad/s at full bank error */
#define AP_PITCH_GAIN 0.5f
#define AP_MIN_AUTHORITY 0.15f
#define AP_STALL_NOSE_DROP 0.6f /* rad/s, wing fully stalled */
#define AP_MIN_TURN_SPEED 18.0f
#define AP_MAX_TURN_RATE 0.8f

#define AP_WIND_STRENGTH 1.5f

#define FUEL_MULT_MIN 0.4f  // drain multiplier at idle/low throttle
#define FUEL_MULT_MAX 4.0f

/* func_8030399C tumbles the vehicle past |pitch| 0.803 or |roll| 1.065:
 * the rates stop short of that. */
#define AP_PITCH_GUARD 0.72f
#define AP_ROLL_GUARD 1.0f

static f32 ap_clamp(f32 v, f32 lo, f32 hi) {
    if (v < lo) {
        return lo;
    }
    if (v > hi) {
        return hi;
    }
    return v;
}

/* ------------------------------------------------------------------------ */
/* Flap modes (sFlapMode: 0 = take off, 1 = cruise, 2 = landing)     */
/* ------------------------------------------------------------------------ */

typedef struct {
    f32 speedKmh;       /* steady speed at refThrottle */
    f32 stallKmh;       /* 1 g stall speed */
    f32 refThrottle;    /* throttle at which speedKmh is reached */
    f32 ctrlSpeedKmh;   /* full control authority from this airspeed */
    f32 rotateMarginKmh;/* elevator bites / liftoff rotation = stall + this */
} FlapSpec;

static const FlapSpec sFlapSpecs[3] = {
    { 110.0f, 80.0f, 0.70f, 80.0f, 5.0f },    /* take off */
    { 230.0f, 155.0f, 0.80f, 180.0f, 10.0f }, /* cruise */
    { 140.0f, 90.0f, 0.50f, 80.0f, 5.0f },   /* landing */
};

typedef struct {
    f32 liftK;
    f32 cd0;
    f32 rotateSpeed; /* m/s */
    f32 ctrlSpeed;   /* m/s */
} FlapParams;

static FlapParams sFlap = { 0.0138f, 0.0015f, 47.22f, 35.0f };

static void flap_params_for(s32 mode, FlapParams* out) {
    const FlapSpec* s;
    f32 vs, v, k, cl, thrust, c;

    if (mode < 0) {
        mode = 0;
    } else if (mode > 2) {
        mode = 2;
    }
    s = &sFlapSpecs[mode];
    vs = KMH_TO_MS(s->stallKmh);
    v = KMH_TO_MS(s->speedKmh);

    /* Stall: K * CL_MAX * vs^2 = g */
    k = AP_GRAVITY / (AP_CL_MAX * SQ(vs));

    /* Level-flight CL at the mode's speed, then the drag that balances
     * thrust there: thrust = (body + CD0) * v^2 + induced. */
    cl = AP_GRAVITY / (k * SQ(v));
    thrust = s->refThrottle * AP_THRUST_MAX * (1.0f - (v / AP_PROP_VMAX));
    c = (thrust - (AP_INDUCED_K * cl * AP_GRAVITY)) / SQ(v);

    out->liftK = k;
    out->cd0 = ap_clamp(c - AP_BODY_DRAG, AP_MIN_CD0, 1.0f);
    out->rotateSpeed = KMH_TO_MS(s->stallKmh + s->rotateMarginKmh);
    out->ctrlSpeed = KMH_TO_MS(s->ctrlSpeedKmh);
}

/* Snap to the current mode (test start / replay). */
static void plane_flap_reset(GyrocopterData* gcData) {
    flap_params_for(sFlapMode, &sFlap);
    PW64_REG(sFlap);
}

/* Ease toward the current mode, once per physics frame. */
static void plane_flap_update(GyrocopterData* gcData) {
    FlapParams t;
    f32 a = ap_clamp(AP_FLAP_RATE * D_8034F854, 0.0f, 1.0f);

    flap_params_for(sFlapMode, &t);
    sFlap.liftK += (t.liftK - sFlap.liftK) * a;
    sFlap.cd0 += (t.cd0 - sFlap.cd0) * a;
    sFlap.rotateSpeed += (t.rotateSpeed - sFlap.rotateSpeed) * a;
    sFlap.ctrlSpeed += (t.ctrlSpeed - sFlap.ctrlSpeed) * a;
}

/* Lift coefficient vs. angle of attack (tan), with a stall. */
static f32 ap_lift_coef(f32 alpha) {
    f32 a = ABS_NOEQ(alpha);
    f32 cl;

    if (a <= AP_ALPHA_STALL) {
        cl = AP_CL_SLOPE * a;
        if (cl > AP_CL_MAX) {
            cl = AP_CL_MAX;
        }
    } else {
        cl = AP_CL_MAX - ((a - AP_ALPHA_STALL) * AP_CL_FALL);
        if (cl < AP_CL_POST_STALL) {
            cl = AP_CL_POST_STALL;
        }
    }
    return (alpha < 0.0f) ? -cl : cl;
}

/* Wing angle of attack (tan) for the current airflow, or 0 below 1 m/s. */
static f32 ap_alpha(GyrocopterData* gcData) {
    f32 airspeed = -gcData->unk364.y;
    f32 alpha;

    if (airspeed < 1.0f) {
        return 0.0f;
    }
    alpha = (gcData->unk364.z / airspeed) + AP_WING_INCIDENCE;
    if (gcData->unkC0 == 1) {
        alpha += AP_GROUND_RAKE;
        /* Takeoff rotation: the main gear holds the attitude, so stick back
         * stands in for the nose coming up (pitch stick back = negative). */
        if ((gcData->unkA0 < 0.0f) && (airspeed > sFlap.rotateSpeed)) {
            alpha -= AP_ROTATE_AOA * gcData->unkA0;
        }
    }
    return ap_clamp(alpha, -1.0f, 1.0f);
}

/* ------------------------------------------------------------------------ */
/* Setup                                                                     */
/* ------------------------------------------------------------------------ */

/* gyrocopterLoadLevel, right after gyrocopterLoadPilot: latch the variant
 * for this test and swap in the plane's model and geometry before the
 * model, capsules (func_80303230) and fx are set up from them. */
void pw64_plane_load(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;
    const PlaneModel* p;
    s32 i;

    if (!pw64_plane_run) {
        /* Not in the attract demo: its recorded flights are gyrocopter ones. */
        if (((pw64_plane != PW64_PLANE_BIG) && (pw64_plane != PW64_PLANE_SMALL)) || D_80362690->unkA4) {
            return;
        }
        sPlane = NULL;
        for (i = (pw64_plane == PW64_PLANE_SMALL) ? 1 : 0; i < ARRAY_COUNT(sPlaneModels); i++) {
            if (plane_model_loaded(sPlaneModels[i].modelId) != NULL) {
                sPlane = &sPlaneModels[i];
                break;
            }
        }
        if (sPlane == NULL) {
            pw64_log("pw64_plane: no plane model in this level, flying the gyrocopter", 0);
            return;
        }
        _uvMediaCopy(&sPlaneResults, &D_80364210[D_80362690->unk9C], sizeof(sPlaneResults));
        pw64_plane_run = 1;
        pw64_log("pw64_plane: test flown by the plane, model", (unsigned int)sPlane->modelId);
    }
    p = sPlane;

    /* The model is drawn as the vehicle and as its wreck (the gyro wreck
     * would be the wrong aircraft); camera props come from it too. */
    gcData->modelId = p->modelId;
    gcData->crashModelId = p->modelId;
    for (i = 0; i < 4; i++) {
        gcData->unk5EC[i] = p->gear[i];
    }
    gcData->unk538.x = 0.0f; /* CG: the vehicle origin */
    gcData->unk538.y = 0.0f;
    gcData->unk538.z = 0.0f;
    gcData->unk5BC = p->bodyFront;
    gcData->unk5C8 = p->bodyRear;
    gcData->unk628 = p->chase.x;
    gcData->unk62C = p->chase.y;
    gcData->unk630 = p->chase.z;
    gcData->unk634 = p->eye.x;
    gcData->unk638 = p->eye.y;
    gcData->unk63C = p->eye.z;

    plane_flap_reset(gcData);
}

/* Root pose with the model's offset (after each uvDobjPosm(objId, 0)). */
void pw64_plane_posm(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;
    Mtx4F m;

    if (!pw64_plane_run || (gcData->objId == 0xFFFF)) {
        return;
    }
    uvMat4Copy(&m, &gcData->unk10);
    uvMat4LocalTranslate(&m, sPlane->offset.x, sPlane->offset.y, sPlane->offset.z);
    uvDobjPosm(gcData->objId, 0, &m);
}

/* gyrocopterEnterLeave, before the camera is placed: a chase distance for
 * the plane's size (the gyro's 1.8..7.9 m would sit inside the big one),
 * the rotor-disc fx hidden, the model posed with its offset. */
void pw64_plane_enter(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;

    if (!pw64_plane_run) {
        return;
    }
    gcData->camera->unk54 = sPlane->camMin;
    gcData->camera->unk58 = sPlane->camMax;
    pw64_plane_props(gc);
    pw64_plane_posm(gc);
}

/* Test setup and leave: a plane run's tallies are not kept (its save is
 * skipped in game.c.patch; this also undoes the in-memory records). */
void pw64_plane_end(void) {
    if (!pw64_plane_run) {
        return;
    }
    _uvMediaCopy(&D_80364210[D_80362690->unk9C], &sPlaneResults, sizeof(sPlaneResults));
    pw64_plane_run = 0;
}

/* ------------------------------------------------------------------------ */
/* Physics (func_8030399C)                                                   */
/* ------------------------------------------------------------------------ */

/* Wind and relative airflow: func_80308478 without the hover lock (the
 * gyro's hands-off level trim, unk7C). */
static void plane_wind(GyrocopterData* gcData) {
    Vec3F local;
    Vec3F world;
    f32 scale;

    if (gcData->unkC0 == 0) {
        env_802E1754(gcData->unk10.m[3][0], gcData->unk10.m[3][1], gcData->unk10.m[3][2], &world);
        uvMat4LocalToWorld(&gcData->unk110, &local, &world);
    } else {
        local.x = local.y = local.z = 0.0f;
        world.x = world.y = world.z = 0.0f;
    }
    gcData->unk7C = 0;

    if (gcData->unk1A8 < 51.587303f) {
        scale = gcData->unk1A8 / 51.587303f;
        scale = SQ(scale);
        local.x *= scale;
        local.y *= scale;
        local.z *= scale;
        world.x *= scale;
        world.y *= scale;
        world.z *= scale;
    }
    gcData->unk364.x = local.x - gcData->unk190.x;
    gcData->unk364.y = local.y - gcData->unk190.y;
    gcData->unk364.z = local.z - gcData->unk190.z;
    gcData->unk31C.x += AP_WIND_STRENGTH * local.x;
    gcData->unk31C.y += AP_WIND_STRENGTH * local.y;
    gcData->unk31C.z += AP_WIND_STRENGTH * local.z;
    gcData->unk1B8.x += AP_WIND_STRENGTH * world.x;
    gcData->unk1B8.y += AP_WIND_STRENGTH * world.y;
    gcData->unk1B8.z += AP_WIND_STRENGTH * world.z;
}

/* Wing lift and drag, fin, and the wing collision capsule (in place of
 * func_80307BAC's rotor). */
static void plane_aero(GyrocopterData* gcData) {
    f32 airspeed;
    f32 cl;
    f32 lift;
    f32 drag;

    /* Wing tip to wing tip, in place of the rotor blade. */
    D_80369070.unk4[1].unk0 = 1;
    D_80369070.unk4[1].unk1C.x = -sPlane->halfSpan;
    D_80369070.unk4[1].unk1C.y = sPlane->wingY;
    D_80369070.unk4[1].unk1C.z = sPlane->wingZ;
    D_80369070.unk4[1].unk28.x = sPlane->halfSpan;
    D_80369070.unk4[1].unk28.y = sPlane->wingY;
    D_80369070.unk4[1].unk28.z = sPlane->wingZ;

    /* No rotor: its rate (the rotor sound's pitch) winds down. */
    gcData->unkE8 = func_80313AF4(0.0f, gcData->unkE8, 0.5f);

    if ((gcData->unkC1 != 0) || (gcData->unkC0 == 2)) {
        return;
    }
    airspeed = -gcData->unk364.y;
    if (airspeed < 1.0f) {
        return;
    }
    cl =ap_lift_coef(ap_alpha(gcData));
    lift = sFlap.liftK * cl * SQ(airspeed);
    /* Landing: on the wheels with the throttle closed the lift is spoiled,
     * so a touchdown sticks instead of floating off the short runways. */
    // if ((gcData->unkC0 == 1) && (gcData->unkB4 < 0.05f)) {
    //     lift *= AP_GROUND_SPOIL;
    // }
    gcData->unk31C.z += lift;
    gcData->unk334.z += lift;

    /* After a touchdown faster than 1 m/s the ground solver (func_80304B98)
     * cancels the lift for 4 s (unk8C counts up) so the gyro settles; a
     * pilot pulling back for takeoff (or a touch-and-go) ends that early. */
    if ((gcData->unkC0 == 1) && (gcData->unkA0 < 0.0f) && (airspeed > sFlap.rotateSpeed) && (gcData->unk8C < 4.0f)) {
        gcData->unk8C = 4.0f;
    }
    drag = (sFlap.cd0 * SQ(airspeed)) + (AP_INDUCED_K * cl * lift);
    gcData->unk31C.y -= drag;

    /* Fin: side airflow pushes the velocity back toward the nose. */
    gcData->unk31C.x += gcData->unk364.x * AP_SIDE_DAMP;
    gcData->unk334.x += gcData->unk364.x * AP_SIDE_DAMP;
}

/* Propeller thrust, fuel and class B refuelling (func_80307EA8 without the
 * rotor lift and exhaust smoke), wheel brakes. */
static void plane_engine(GyrocopterData* gcData) {
    f32 altFactor;
    f32 thrust;
    f32 speed;

    if ((gcData->unkC1 != 0) || (gcData->unkC0 == 2)) {
        gcData->unkF0 = func_80313AF4(0.0f, gcData->unkF0, 0.5f);
        return;
    }
    if (D_8036957E == CLASS_B) {
        if (func_802E08F4(gcData->unk10.m[3][0], gcData->unk10.m[3][1], gcData->unk10.m[3][2]) != 0) {
            if (gcData->unk67E == 0) {
                gcData->unk67E = 1;
                sndPlaySfx(SFX_UI_FUEL_ADDED);
            }
            hudWarningText(-1, 0.0f, 0.0f);
            gcData->hasLowFuel = FALSE;
            gcData->fuelEmpty = FALSE;
            gcData->fuel = 1.0f;
        } else {
            gcData->unk67E = 0;
        }
    }
    if ((gcData->fuel > 0.0f) && gcData->usingFuel) {
        f32 throttle = ap_clamp(gcData->unkB4, 0.0f, 1.0f);
        f32 t2 = throttle * throttle;
        f32 t4 = t2 * t2;
        f32 t8 = t4 * t4; // throttle^8, no powf needed
        f32 fuelMultiplier = FUEL_MULT_MIN + (FUEL_MULT_MAX - FUEL_MULT_MIN) * t8;

        gcData->fuel -= func_80313F08(&D_803694D0, throttle) * fuelMultiplier * D_8034F854;
        if (gcData->fuel <= 0.0f) {
            gcData->usingFuel = FALSE;
        }
        gcData->fuel = ap_clamp(gcData->fuel, 0.0f, 1.0f);
    }

    /* Thin air above 700 m, as for the rotor. */
    altFactor = 1.0f;
    if (gcData->unk10.m[3][2] > 700.0f) {
        altFactor = ap_clamp(1.0f - ((gcData->unk10.m[3][2] - 700.0f) / 100.0f), 0.0f, 1.0f);
    }
    if (gcData->usingFuel) {
        speed = ap_clamp(-gcData->unk364.y, 0.0f, AP_PROP_VMAX);
        thrust = gcData->unkB4 * AP_THRUST_MAX * altFactor * (1.0f - (speed / AP_PROP_VMAX));
        gcData->unk31C.y += thrust;
        gcData->unk334.y += thrust;
        /* Propeller rate: drives the engine sound like the gyro's pusher. */
        gcData->unkF0 = func_80313AF4((gcData->unkB4 * 94.24764f) + 10.47196f, gcData->unkF0, 1.0f);
    } else {
        gcData->unkF0 = func_80313AF4(0.0f, gcData->unkF0, 0.5f);
    }

    /* Wheel brakes with the throttle closed; tapering under 1 m/s so the
     * plane comes to rest (the test ends below 0.1 m/s). */
    if ((gcData->unkC0 == 1) && (gcData->unkB4 < 0.05f) && (gcData->unk190.y > 0.0f)) {
        gcData->unk31C.y -= ap_clamp(gcData->unk190.y, 0.0f, 1.0f) * AP_BRAKE;
    }
}

/* func_8030399C: wind, wing, propeller in place of func_80308478,
 * func_80307BAC and func_80307EA8. */
int pw64_plane_forces(struct GyrocopterData* gc) {
    if (!pw64_plane_run) {
        return 0;
    }
    plane_flap_update(gc);
    plane_wind(gc);
    plane_aero(gc);
    plane_engine(gc);
    return 1;
}

/* Airborne attitude control (state 0) in place of func_80304738: the
 * stick holds a bank and a pitch angle, turns are coordinated. unk9C =
 * roll stick, unkA0 = pitch stick (forward +), both shaped and signed. */
int pw64_plane_control(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;
    f32 diff;
    f32 absDiff;
    f32 airspeed;
    f32 authority;
    f32 alpha;
    f32 stall;
    f32 cosBank;
    f32 turnSpeed;
    f32 yawCos;

    if (!pw64_plane_run) {
        return 0;
    }
    gcData->unk204 = uvSinF(gcData->unk1EC);
    gcData->unk208 = uvCosF(gcData->unk1EC);
    gcData->unk20C = uvSinF(gcData->unk1F0);
    gcData->unk210 = uvCosF(gcData->unk1F0);
    gcData->unk214 = uvSinF(gcData->unk1F4);
    gcData->unk218 = uvCosF(gcData->unk1F4);

    /* Slew-limit the sticks, as func_80304738 does. */
    diff = gcData->unk9C - gcData->unkA4;
    absDiff = ABS_NOEQ(diff);
    if ((absDiff / D_8034F854) > 5.0f) {
        gcData->unk9C = gcData->unkA4 + ((5.0f * D_8034F854 * diff) / absDiff);
    }
    diff = gcData->unkA0 - gcData->unkA8;
    absDiff = ABS_NOEQ(diff);
    if ((absDiff / D_8034F854) > 5.0f) {
        gcData->unkA0 = gcData->unkA8 + ((5.0f * D_8034F854 * diff) / absDiff);
    }
    gcData->unkA4 = gcData->unk9C;
    gcData->unkA8 = gcData->unkA0;

    airspeed = ap_clamp(-gcData->unk364.y, 0.0f, 1000.0f);
        authority = ap_clamp(airspeed / sFlap.ctrlSpeed, AP_MIN_AUTHORITY, 1.0f);
    alpha = ap_alpha(gcData);
    stall = ap_clamp((ABS_NOEQ(alpha) - AP_ALPHA_STALL) / 0.15f, 0.0f, 1.0f);

    /* Roll: bank toward stick^2 * max bank; a stalled wing answers less. */
    gcData->unk200 = ((ABS_NOEQ(gcData->unk9C) * gcData->unk9C) - (gcData->unk1F4 / AP_MAX_BANK)) * AP_ROLL_GAIN *
                     authority * (1.0f - (0.5f * stall));

    /* Pitch: toward -stick^2 * max pitch (back = nose up), less effective
     * banked; past the stall the nose drops. */
    gcData->unk1FC = ((ABS_NOEQ(gcData->unkA0) * gcData->unkA0) + (gcData->unk1F0 / AP_MAX_PITCH)) *
                     (-AP_PITCH_GAIN * authority * gcData->unk218);
    gcData->unk1FC -= AP_STALL_NOSE_DROP * stall;

    /* Stay clear of the tumble test (func_8030399C). */
    if (((gcData->unk1F0 > AP_PITCH_GUARD) && (gcData->unk1FC > 0.0f)) ||
        ((gcData->unk1F0 < -AP_PITCH_GUARD) && (gcData->unk1FC < 0.0f))) {
        gcData->unk1FC = 0.0f;
    }
    if (((gcData->unk1F4 > AP_ROLL_GUARD) && (gcData->unk200 > 0.0f)) ||
        ((gcData->unk1F4 < -AP_ROLL_GUARD) && (gcData->unk200 < 0.0f))) {
        gcData->unk200 = 0.0f;
    }

    /* Yaw: coordinated turn, rate = g * tan(bank) / speed. */
    if (airspeed > 0.5f) {
        cosBank = (gcData->unk218 < 0.5f) ? 0.5f : gcData->unk218;
        turnSpeed = (airspeed > AP_MIN_TURN_SPEED) ? airspeed : AP_MIN_TURN_SPEED;
        gcData->unk1F8 = ap_clamp(-(AP_GRAVITY * gcData->unk214) / (cosBank * turnSpeed), -AP_MAX_TURN_RATE,
                                  AP_MAX_TURN_RATE);
    } else {
        gcData->unk1F8 = 0.0f;
    }

    /* Euler rates -> body rates (as func_80304738). */
    yawCos = gcData->unk1F8 * gcData->unk210;
    gcData->unk1D4 = (gcData->unk1FC * gcData->unk218) - (yawCos * gcData->unk214);
    gcData->unk1D8 = gcData->unk200 + (gcData->unk1F8 * gcData->unk20C);
    gcData->unk1DC = (gcData->unk1FC * gcData->unk214) + (yawCos * gcData->unk218);
    return 1;
}

/* Airborne acceleration in place of func_80309090: the summed forces as
 * they are, without the gyro's autorotation descent, hover trim and speed
 * cap. On the ground (unkC0 != 0) func_80309090 runs as usual. */
int pw64_plane_accel(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;

    if (!pw64_plane_run || (gcData->unkC0 != 0)) {
        return 0;
    }
    gcData->unk1AC = gcData->unk31C;
    gcData->unk6D = 0;
    gcData->unk6E = 0;
    gcData->unk90 = 0.0f;
    return 1;
}

/* ------------------------------------------------------------------------ */
/* Model pose (code_7C4C0.c)                                                 */
/* ------------------------------------------------------------------------ */

/* func_802F5358 (rotor fx, rotor and pusher parts): the plane models have
 * one part, so only the spin angles advance and the rotor-disc fx (created
 * by func_802F4F98, freed by func_802F5304 as usual) stay invisible. */
int pw64_plane_props(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;

    if (!pw64_plane_run) {
        return 0;
    }
    gcData->unkF4 += gcData->unkF0 * D_8034F854;
    gcData->unkEC += gcData->unkE8 * D_8034F854;
    uvFxProps(gcData->unk660, FX_PROP_5(gcData->unk578, gcData->unk57C, gcData->unk580, 0.0f), FX_PROP_END);
    uvFxProps(gcData->unk661, FX_PROP_5(gcData->unk584, gcData->unk588, gcData->unk58C, 0.0f), FX_PROP_END);
    return 1;
}

/* func_802F50CC: no rotor, stick or tail parts to pose (their part indices
 * are the gyro model's); keeps the damaged-engine smoke puffs. */
int pw64_plane_pose(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;

    if (!pw64_plane_run) {
        return 0;
    }
    if (gcData->objId == 0xFFFF) {
        return 1;
    }
    pw64_plane_props(gc);
    if ((gcData->unkC0 != 3) && ((gcData->unkC1 == 8) || (gcData->unkC1 == 1)) && (gcData->unk1A8 > 4.0f) &&
        ((D_8034F850 - gcData->unkE4) > 1.0f)) {
        func_802F8B0C(gcData->unk10.m[3][0], gcData->unk10.m[3][1], gcData->unk10.m[3][2], 4.0f);
        gcData->unkE4 = D_8034F850;
    }
    return 1;
}

/* func_802F56B4 (results, after landing): the gyro pilot's wave animation
 * poses gyro parts; the plane only spools down. */
int pw64_plane_rest(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;

    if (!pw64_plane_run) {
        return 0;
    }
    gcData->usingFuel = FALSE;
    gcData->unkE8 = func_80313AF4(0.0f, gcData->unkE8, 0.5f);
    gcData->unkF0 = func_80313AF4(0.0f, gcData->unkF0, 0.5f);
    pw64_plane_props(gc);
    return 1;
}

// in func_8030399C gcData->unkC0 fly mode: 0, on ground: 1, on ground and stopped 4.
void pw64_plane_ground_physics(struct GyrocopterData* gc) {
    GyrocopterData* gcData = gc;
    s32 i;
    s32 j;
    s32 sp184;
    s32 temp_s2;
    f32 sp170[4];
    f32 temp_fs0;
    f32 temp_fs0_3;
    f32 sp164;
    f32 temp_fs1;
    f32 temp_fv0_2;
    f32 temp_fv0_4;
    f32 temp_fv0_6;
    f32 sp150;
    f32 temp_fv1;
    f32 temp_ft5;
    f32 var_fv1;
    f32 temp_fa0_2;
    f32 sp13C;
    f32 sp138;
    f32 temp_fv1_2;
    f32 sp130;
    f32 sp12C;
    f32 sp128;
    Vec3F sp11C;
    Vec3F sp110;
    Vec3F sp104;
    Vec3F spF8;
    Vec3F spEC;
    Vec3F spE0;
    Vec3F spD4;
    Mtx4F sp94;

    if (gcData->unk8C < 4.0f) {
        if (gcData->unk510 > 0) {
            for (i = 0; i < gcData->unk510; i++) {
                sp184 = gcData->unk514[i];
                if (i == 0) {
                    uvVec3Copy(&sp110, &gcData->unk3B0[sp184].unk10);
                } else {
                    uvVec3Add(&sp110, &sp110, &gcData->unk3B0[sp184].unk10);
                }
            }
            uvVec3Mul(&sp110, &sp110, 1.0f / gcData->unk510);
            uvVec3Normal(&sp110, &sp110);
        } else {
            sp110.x = 0.0f;
            sp110.y = 0.0f;
            sp110.z = 1.0f;
            uvMat4LocalToWorld(&gcData->unk110, &sp110, &sp110);
        }
        if (gcData->unk8C > 2.0f) {
            uvVec3Mul(&sp11C, &sp110, (uvVec3Dot(&sp110, &gcData->unk334) * (gcData->unk8C - 4.0f)) / 2);
        } else {
            uvVec3Mul(&sp11C, &sp110, -uvVec3Dot(&sp110, &gcData->unk334));
        }

        uvVec3Add(&gcData->unk31C, &gcData->unk31C, &sp11C);
        gcData->unk8C += D_8034F854;
    }
    gcData->unk524 = 0;

    for (i = 0; i < gcData->unk510; i++) {
        sp184 = gcData->unk514[i];
        uvVec3Cross(&sp11C, &gcData->unk1C8, &gcData->unk5EC[sp184]);
        uvVec3Cross(&sp11C, &gcData->unk1C8, &sp11C);
        sp170[gcData->unk524] = -uvVec3Dot(&sp11C, &gcData->unk3B0[sp184].unk10);
        sp170[gcData->unk524] -= uvVec3Dot(&gcData->unk31C, &gcData->unk3B0[sp184].unk10);
        if ((gcData->unk3B0[sp184].unk54 < 0.01f) && ((gcData->unk8C <= 2.0f) || ((gcData->unk8C > 2.0f) && (sp170[gcData->unk524] > 0.0f)))) {
            gcData->unk528[gcData->unk524++] = sp184;
        }
    }

    if (gcData->unk524 == 0) {
        return;
    }

    if (gcData->unk524 < 3) {
        for (i = 0; i < gcData->unk524; i++) {
            sp184 = gcData->unk528[i];
            for (j = 0; j < i; j++) {
                temp_s2 = gcData->unk528[j];
                uvVec3Cross(&sp11C, &gcData->unk3B0[temp_s2].unk28, &gcData->unk5EC[sp184]);
                sp94.m[j][i] = sp94.m[i][j] =
                    (uvVec3Dot(&gcData->unk3B0[sp184].unk10, &gcData->unk3B0[temp_s2].unk10) / 250.0f) + uvVec3Dot(&gcData->unk3B0[sp184].unk10, &sp11C);
            }

            uvVec3Cross(&sp11C, &gcData->unk3B0[sp184].unk28, &gcData->unk5EC[sp184]);
            sp94.m[i][i] = uvVec3Dot(&gcData->unk3B0[sp184].unk10, &sp11C) + 0.004f;
        }
        for (i = 0; i < gcData->unk524; i++) {
            sp184 = gcData->unk528[i];
            // for (j = 0; j < gcData->unk524; j++) {
            //     temp_s2 = gcData->unk528[j];
            //     if (i == j) {
            //         uvVec3Cross(&sp11C, &gcData->unk3B0[sp184].unk34, &gcData->unk5EC[sp184]);
            //         sp94.m[i][j] += -uvVec3Dot(&gcData->unk3B0[sp184].unk10, &sp11C) * gcData->unk3B0[temp_s2].unk40;
            //     } else {
            //         uvVec3Cross(&sp11C, &gcData->unk3B0[temp_s2].unk34, &gcData->unk5EC[sp184]);
            //         sp94.m[i][j] += -((uvVec3Dot(&gcData->unk3B0[sp184].unk10, &gcData->unk3B0[temp_s2].unk1C) / 250.0f) +
            //                           uvVec3Dot(&gcData->unk3B0[sp184].unk10, &sp11C)) *
            //                         gcData->unk3B0[temp_s2].unk40;
            //     }
            // }
        }
    }
    if (gcData->unk524 < 3) {
        if (gcData->unk524 == 1) {
            sp170[0] /= sp94.m[0][0];
        } else {
            temp_fv0_2 = (sp94.m[0][0] * sp94.m[1][1]) - (sp94.m[0][1] * sp94.m[1][0]);
            temp_fv1_2 = ((sp170[0] * sp94.m[1][1]) - (sp170[1] * sp94.m[0][1])) / temp_fv0_2;
            sp170[1] = ((sp170[1] * sp94.m[0][0]) - (sp170[0] * sp94.m[1][0])) / temp_fv0_2;
            sp170[0] = temp_fv1_2;
        }

        for (i = 0; i < gcData->unk524; i++) {
            sp184 = gcData->unk528[i];
            uvVec3Mul(&sp11C, &gcData->unk3B0[sp184].unk10, sp170[i] / 250.0f);
            uvVec3Add(&gcData->unk31C, &gcData->unk31C, &sp11C);
            uvVec3Mul(&sp11C, &gcData->unk3B0[sp184].unk28, sp170[i]);
            uvVec3Add(&gcData->unk328, &gcData->unk328, &sp11C);
        }




    } else {
        for (i = 0; i < gcData->unk524; i++) {
            sp184 = gcData->unk528[i];
            if (i == 0) {
                sp164 = gcData->unk3B0[sp184].unk40;
                uvVec3Copy(&sp110, &gcData->unk3B0[sp184].unk10);
            } else {
                sp164 += gcData->unk3B0[sp184].unk40;
                uvVec3Add(&sp110, &sp110, &gcData->unk3B0[sp184].unk10);
            }
        }
        if (uvVec3Dot(&gcData->unk31C, &sp110) < 0.0f) {
            uvVec3Mul(&sp110, &sp110, 1.0f / gcData->unk524);
            uvVec3Normal(&sp110, &sp110);
            uvVec3Cross(&sp104, &sp110, &gcData->unk190);
            uvVec3Cross(&sp104, &sp104, &sp110);
            sp13C = uvSqrtF(SQ(sp110.y) + SQ(sp110.z));
            sp138 = sp110.z / sp13C;
            sp130 = -sp110.y / sp13C;
            sp12C = sp110.x * sp130;
            sp128 = sp110.x * sp138;
            spEC.x = sp13C;
            spEC.y = sp12C;
            spEC.z = -sp128;
            spF8.x = 0.0f;
            spF8.y = sp138;
            spF8.z = sp130;
            temp_fv0_4 = uvVec3Dot(&sp104, &spF8);
            var_fv1 = 0;// (1.0f - gcData->unkB4) * temp_fv0_4 * 0.3f;
            if (gcData->unk6C != 0) {
                var_fv1 *= 0.8f;
            }
            if (temp_fv0_4 < 2.0f) {
                var_fv1 *= 1.0f + (4.0f * (2.0f - temp_fv0_4));
            }
            var_fv1 = 0; // This removes immediate slow down on landing.
            uvVec3Mul(&spD4, &spF8, -var_fv1);

            for (i = 0; i < 3; i++) {
                sp184 = gcData->unk528[i];
                if ((sp184 == 0) || (sp184 == 3)) {
                    if (gcData->unk6C == 0) {
                        sp150 = (gcData->unkAC + gcData->unk9C) * -0.2617994f;
                    } else {
                        if (((gcData->unk190.y * 0.7f) > 40.0f) && ((gcData->unk190.y * 0.7f) <= 45.0f)) {
                            sp150 = ((gcData->unkAC + gcData->unk9C) * -0.0872665f * ((gcData->unk190.y * 0.7f) - 40.0f)) / 5.0f;
                        } else if (((gcData->unk190.y * 0.7f) > 45.0f) && ((gcData->unk190.y * 0.7f) < 50.0f)) {
                            sp150 = ((gcData->unkAC + gcData->unk9C) * -0.0872665f * (50.0f - (gcData->unk190.y * 0.7f))) / 5.0f;
                        } else {
                            sp150 = 0.0f;
                        }
                    }
                    if (sp184 == 3) {
                        sp150 = -sp150;
                    }
                    temp_fs0_3 = uvCosF(sp150);
                    temp_fv0_6 = uvSinF(sp150);
                    spEC.x = temp_fs0_3 * sp13C;
                    spEC.y = (temp_fs0_3 * sp12C) + (temp_fv0_6 * sp138);
                    spEC.z = (-temp_fs0_3 * sp128) + (temp_fv0_6 * sp130);
                    spF8.x = -temp_fv0_6 * sp13C;
                    spF8.y = (-temp_fv0_6 * sp12C) + (temp_fs0_3 * sp138);
                    spF8.z = (temp_fv0_6 * sp128) + (temp_fs0_3 * sp130);
                } else {
                    spEC.x = sp13C;
                    spEC.y = sp12C;
                    spEC.z = -sp128;
                    spF8.x = 0.0f, spF8.y = sp138, spF8.z = sp130;
                }

                temp_fs1 = gcData->unk3B0[sp184].unk50 * uvVec3Dot(&gcData->unk3B0[sp184].unk1C, &spF8);
                temp_fa0_2 = gcData->unk3B0[sp184].unk50 * uvVec3Dot(&gcData->unk3B0[sp184].unk1C, &spEC);
                if (gcData->unk528[0] == 0) {
                    temp_fv1 = gcData->unk3B0[sp184].unk48 * temp_fs1 * 0.35f;
                    temp_fs0 = gcData->unk3B0[sp184].unk48 * 0.5f * temp_fa0_2 * 2.4f;
                } else {
                    temp_fv1 = gcData->unk3B0[sp184].unk4C * temp_fs1 * 0.35f;
                    temp_fs0 = gcData->unk3B0[sp184].unk4C * 0.5f * temp_fa0_2 * 2.4f;
                }
                if ((gcData->unk6C == 0) && (gcData->unk50 != -1)) {
                    temp_fv1 *= 1.1f;
                    temp_fs0 *= 1.1f;
                }
                uvVec3Mul(&spF8, &spF8, -temp_fv1);
                uvVec3Add(&spD4, &spD4, &spF8);
                uvVec3Mul(&spEC, &spEC, -temp_fs0);
                uvVec3Add(&spD4, &spD4, &spEC);
                spE0.x = gcData->unk5EC[sp184].x - gcData->unk538.x;
                if (gcData->unk528[0] == 0) {
                    spE0.y = gcData->unk5EC[sp184].y - gcData->unk538.y;
                } else {
                    spE0.y = gcData->unk5EC[sp184].y + gcData->unk538.y;
                }
                spE0.z = gcData->unk5EC[sp184].z - gcData->unk538.z;
                uvVec3Cross(&spE0, &sp110, &spE0);
                uvVec3Cross(&spE0, &spE0, &sp110);
                uvVec3Cross(&spF8, &spE0, &spF8);
                uvVec3Add(&gcData->unk328, &gcData->unk328, &spF8);
                uvVec3Cross(&spEC, &spE0, &spEC);
                uvVec3Add(&gcData->unk328, &gcData->unk328, &spEC);
            }
            uvVec3Cross(&gcData->unk31C, &sp110, &gcData->unk31C);
            uvVec3Cross(&gcData->unk31C, &gcData->unk31C, &sp110);
            uvVec3Add(&gcData->unk31C, &gcData->unk31C, &spD4);
            uvVec3Mul(&gcData->unk328, &sp110, uvVec3Dot(&gcData->unk328, &sp110));
        }
    }
}

// From gyrocopterMovementFrame in code_7CF30.c
void pw64_plane_movement_frame(GyrocopterData* gcData, u8 gameState) {
    s32 pad[2];
    HUDState* hud;
    f32 xAxisInput;
    f32 yAxisInput;
    s32 buttons;
    u8 sp67;
    s32 temp_v0_3;
    f32 var_fa0;
    f32 var_fa0_2;
    f32 var_fv0_4;
    f32 sp48[3];

    if (gcData->unkC0 == 3) {
        func_802F764C(gcData);
        return;
    }

    if (fdr_802E6B5C() != 4) {
        shadow_803343D8(1);

        if (gameState == GAME_STATE_RESULTS) {
            fdr_802E65AC(&gcData->unk10, &D_80362690->terraId, &xAxisInput, &yAxisInput, &buttons);
        } else {
            xAxisInput = demoGetInputs(gcData->contIdx, INPUT_AXIS_X);
            yAxisInput = demoGetInputs(gcData->contIdx, INPUT_AXIS_Y);
            buttons = demoGetButtons(gcData->contIdx);
        }

        if ((gcData->elapsedTime > 0.0f) || (buttons & A_BUTTON)) {
            gcData->elapsedTime += D_8034F854;
        }

        gcData->unk9C = func_80313F08(&D_80368880, ABS_NOEQ(xAxisInput));
        if (xAxisInput < 0.0f) {
            gcData->unk9C = -gcData->unk9C;
        }

        gcData->unkA0 = func_80313F08(&D_80368880, ABS_NOEQ(yAxisInput));
        if (yAxisInput < 0.0f) {
            gcData->unkA0 = -gcData->unkA0;
        }

        if ((gcData->unkC0 == 0) || (gcData->unkC0 == 2)) {
            gcData->unk5F = 1;
        } else {
            if (gcData->unk5F != 0) {
                gcData->unk5F = 2;
            }
        }
        switch (gcData->unk5F) {
        case 0:
            sFlapMode = 0; // Take off setting
            if (buttons & A_BUTTON) {
                gcData->unkB0 += gcData->unk654 * D_8034F854;
                if (!gcData->usingFuel) {
                    gcData->usingFuel = TRUE;
                }
            }
            if (buttons & B_BUTTON) {
                gcData->unkB0 -= gcData->unk658 * D_8034F854;
            }
            if (!(buttons & (A_BUTTON | B_BUTTON))) {
                gcData->unkB0 -= gcData->unk65C * D_8034F854;
            }

            if (gcData->unkB0 < 0.0f) {
                gcData->unkB0 = 0.0f;
            } else if (gcData->unkB0 > 1.0f) {
                gcData->unkB0 = 1.0f;
            }
            gcData->unk9C = 0.0f;
            if (gcData->fuel <= 0.0f) {
                gcData->unk5F = 2;
            }
            break;
        case 1:
        case 2:
            if ((gcData->unk5F == 2) && (gcData->unkB0 == 0.0f) && (gcData->unk1A8 < 0.1f)) {
                gcData->unkB0 = 0.0f;
                gcData->usingFuel = FALSE;
            } else {
                if (buttons & A_BUTTON) {
                    gcData->unkB0 += gcData->unk648 * D_8034F854;
                    if (!gcData->usingFuel && (gcData->fuel > 0.0f)) {
                        gcData->usingFuel = TRUE;
                    }
                }
                if (buttons & B_BUTTON) {
                    gcData->unkB0 -= gcData->unk64C * D_8034F854;
                }
                
                // Activate next flap mode
                if (demoButtonPress(gcData->contIdx, U_JPAD)) {
                    if (sFlapMode < 2) {
                        sFlapMode += 1;
                    }
                }

                // Activate previous flap mode
                if (demoButtonPress(gcData->contIdx, D_JPAD)) {
                    if (sFlapMode > 0) {
                        sFlapMode -= 1;
                    }
                }

                // if (!(buttons & (A_BUTTON | B_BUTTON))) { //     if (gcData->unkB0 > 0.5f) { //         gcData->unkB0 -= gcData->unk650 * D_8034F854;
                //         if (gcData->unkB0 < 0.5f) {
                //             gcData->unkB0 = 0.5f;
                //         }
                //     } else if (gcData->unkB0 < 0.5f) {
                //         gcData->unkB0 += gcData->unk650 * D_8034F854;
                //         if (gcData->unkB0 > 0.5f) {
                //             gcData->unkB0 = 0.5f;
                //         }
                //     }
                // }
            }
            if (gcData->unkB0 < 0.0f) {
                gcData->unkB0 = 0.0f;
            } else if (gcData->unkB0 > 1.0f) {
                gcData->unkB0 = 1.0f;
            }
            break;
        }
        gcData->unkB4 = gcData->unkB0;
        if ((gcData->unkC0 == 0) && (gcData->unkDC > 1.0f) && (ABS_NOEQ(gcData->unkB8) <= 0.12217303f) && (ABS_NOEQ(gcData->unkBC) <= 0.12217303f)) {
            temp_v0_3 = func_80314114(D_8034F380, buttons, Z_TRIG);
            switch (temp_v0_3) {
            default:
                break;
            case 1:
                if (func_802F6E74(gcData) >= 0) {
                    func_802F6DC8(gcData, 1);
                }
                break;
            case 0:
                if ((buttons & Z_TRIG) && (func_802F6E74(gcData) >= 0)) {
                    func_802F6DC8(gcData, 1);
                }
                break;
            case -1:
                if (gcData->unk5E != 0) {
                    func_802F6DC8(gcData, 0);
                    func_802F6EE0(gcData);
                }
                break;
            }
        } else {
            func_802F6DC8(gcData, 0);
        }
        if (buttons & Z_TRIG) {
            sp67 = 1;
        } else {
            sp67 = 0;
        }
        if (buttons & R_CBUTTONS) {
            if (!sp67) {
                var_fa0 = -1.5707961f;
            } else {
                var_fa0 = -0.10471974f;
            }
        } else if (buttons & L_CBUTTONS) {
            if (!sp67) {
                var_fa0 = 1.5707961f;
            } else {
                var_fa0 = 0.10471974f;
            }
        } else {
            var_fa0 = 0.0f;
        }
        if (!sp67) {
            var_fv0_4 = 2.0f;
        } else {
            var_fv0_4 = 3.0f;
        }
        if (!sp67) {
            gcData->unkB8 = func_80313BAC(var_fa0, gcData->unkB8, var_fv0_4);
        } else {
            gcData->unkB8 = func_80313AF4(var_fa0, gcData->unkB8, var_fv0_4);
        }

        if (buttons & U_CBUTTONS) {
            if (!sp67) {
                var_fa0 = -1.5707961f;
            } else {
                var_fa0 = -0.10471974f;
            }
        } else if (buttons & D_CBUTTONS) {
            if (!sp67) {
                var_fa0 = 1.5707961f;
            } else {
                var_fa0 = 0.10471974f;
            }
        } else {
            var_fa0 = 0.0f;
        }
        if (!sp67) {
            var_fv0_4 = 2.0f;
        } else {
            var_fv0_4 = 3.0f;
        }
        if (!sp67) {
            gcData->unkBC = func_80313BAC(var_fa0, gcData->unkBC, var_fv0_4);
        } else {
            gcData->unkBC = func_80313AF4(var_fa0, gcData->unkBC, var_fv0_4);
        }

        if (gameState != GAME_STATE_RESULTS) {
            func_8030399C(gcData);
        }
        func_802F50CC(gcData);
        func_802F7224(gcData);
        uvDobjPosm(gcData->objId, 0, &gcData->unk10);
        if (gameState != GAME_STATE_RESULTS) {
            sp48[0] = gcData->unkDC;
            sp48[1] = gcData->unkE8;
            sp48[2] = gcData->unkF0;
            fdr_802E682C(sp48, ARRAY_COUNT(sp48), 0);
        } else {
            fdr_802E6870(sp48, ARRAY_COUNT(sp48), NULL);
            gcData->unkDC = sp48[0];
            gcData->unkE8 = sp48[1];
            gcData->unkF0 = sp48[2];
        }
        if ((demoButtonPress(gcData->contIdx, R_TRIG) != 0) && (gameState != GAME_STATE_RESULTS)) {
            if (gcData->unk5C == 1) {
                sndPlaySfxVolPitchPan(0x6A, 1.0f, 0.5f, 0.0f);
                gcData->unk5C = 0;
                gcData->unk68 = 0.8f;
            } else {
                sndPlaySfxVolPitchPan(0x6A, 1.0f, 0.5f, 0.0f);
                gcData->unk5C = 1;
                gcData->unk68 = 0.8f;
            }
        }
        if (gameState != GAME_STATE_RESULTS) {
            if ((gcData->unkC0 == 2) || (gcData->unkC0 == 3)) {
                camera_802D5884(gcData->camera, 6);
            } else {
                camera_802D5884(gcData->camera, gcData->unk5C);
            }
            camera_802D45C4(gcData->camera, gcData->unk68);
        } else {
            camera_802D5884(gcData->camera, 5);
        }
        if (D_80362690->unkC[D_80362690->unk9C].unk7B != 0) {
            gcData->fuel = 1.0f;
        }
        if ((gcData->unk5C == 1) && (gcData->unkC0 != 2) && (gameState != GAME_STATE_RESULTS)) {
            gcData->unk5D = 0;
        } else {
            gcData->unk5D = 1;
        }
        if (gcData->unk5D != 0) {
            gcData->unk2 |= 2;
            uvDobjState(gcData->objId, gcData->unk2);
        } else {
            if (gcData->unk2 & 2) {
                gcData->unk2 &= ~2;
            }
            uvDobjState(gcData->objId, gcData->unk2);
        }
        if (gameState != GAME_STATE_RESULTS) {
            gcData->camera->unk1A4 = gcData->unk190.y;
            gcData->camera->unk78 = gcData->unkB8;
            gcData->camera->unk7C = gcData->unkBC;
            gcData->camera->unk6 = gcData->unk2;
            gcData->camera->unk228 = gcData->unkDC;
            uvMat4Copy(&gcData->camera->unk80, &gcData->unk10);
            hud = hudGetState();
            uvMat4Copy(&hud->unk28, &gcData->unk10);
            hud->renderFlags = HUD_RENDER_GYROCOPTER;
            if (gcData->unk5E != 0) {
                hud->renderFlags |= HUD_RENDER_RETICLE;
            } else {
                if (hud->renderFlags & HUD_RENDER_RETICLE) {
                    hud->renderFlags &= ~HUD_RENDER_RETICLE;
                }
            }
            hud->reticleX = gcData->reticleX;
            hud->reticleY = gcData->reticleY;
            hud->elapsedTime = gcData->elapsedTime;
            hud->fuel = gcData->fuel;
            hud->att.heading = gcData->unk10.m[3][2];
            hud->unk8C = gcData->unk19C.z * 16.0f * 0.7f;
            hud->altitude = gcData->unkDC * 0.7f;
            hud->altSeaLevel = gcData->unk10.m[3][2] * 0.7f;
            hud->speed = gcData->unk1A8 * 3.6f * 0.7f;

            if (hud->fuel > 0.0f) {
                hud->power = gcData->unkB4;
            } else {
                hud->power -= D_8034F854;
                if (hud->power < 0.0f) {
                    hud->power = 0.0f;
                } else if (hud->power > 1.0f) {
                    hud->power = 1.0f;
                }
            }
            if (hud->power < 0.0f) {
                hud->power = 0.0f;
            }
            if (gcData->unkC1 != 0) {
#if defined(VERSION_JP)
                hudText_8031D8E0(TEXT_OFF_5, 1.5f, 8.0f);
#else // VERSION_US
                hudWarningText(TEXT_OFF_5, 1.5f, 8.0f);
#endif
                sndPlaySfx(SFX_UI_POINT_PENALTY);
            } else {
                if ((gcData->fuel <= 0.0f) && !gcData->fuelEmpty) {
                    hudWarningText(TEXT_FUEL_OUT, 1.5f, 8.0f);
                    gcData->fuelEmpty = TRUE;
                } else if ((gcData->fuel < 0.15f) && !gcData->hasLowFuel) {
                    hudWarningText(TEXT_FUEL_WAR, 1.5f, 8.0f);
                    gcData->hasLowFuel = TRUE;
                } else if ((func_8032C080(NULL) != 0) && !gcData->pleaseLandShown) {
                    sndPlaySfx(SFX_UI_HUD_CHIME);
                    hudText_8031D8E0(TEXT_LAND_NOW2, 1.5f, 8.0f);
                    gcData->pleaseLandShown = TRUE;
                } else if ((taskGet_80346468() != 0) && !gcData->unkD5) {
                    hudText_8031D8E0(TEXT_GOAL, 1.5f, 8.0f);
                    gcData->unkD5 = TRUE;
                }
            }
        }

        if ((gcData->unkC0 == 3) || (gcData->unkC0 == 2)) {
            hud->renderFlags = 0;
        }
        shadow_803344BC(&gcData->unk10, gcData->unkDC);
        D_8034F380 = buttons;
        func_802E06AC(&gcData->unk10);
        if (gameState != GAME_STATE_RESULTS) {
            if (gcData->unkC0 == 3) {
                fdr_802E66DC();
            }
            fdr_802E65AC(&gcData->unk10, &D_80362690->terraId, &xAxisInput, &yAxisInput, &buttons);
        }
    }
}

// Draws the flap mode text
void pw64_plane_draw_hud(void) {
    const char* str = "";
    switch (sFlapMode) {
        case 0:
        str = "TAKE OFF";
        break;
        case 1:
        str = "CRUISE";
        break;
        case 2:
        str = "LAND";
        break;
    }

    s32 half = (s32) (SCREEN_WIDTH / 2);
    pw64_hud_anchor_xy(half, 37);
    pw64_hud_element(PW64_HUD_FLAPS); // PW64: movable HUD

    uvFontSet(6);
    uvFontColor(10, 10, 0xFF, 0xFF);
    uvFontScale(0.5f, 0.5f);

    uvFontPrintStr(half - 30, 37, "FLAPS");
    uvFontScale(0.5f, 0.5f);
    uvFontPrintStr(half + 5, 37, str);
}
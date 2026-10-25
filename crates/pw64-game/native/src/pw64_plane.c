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
#include "app/environment.h"
#include "app/game.h"
#include "app/gyrocopter.h"
#include "app/hud.h"
#include "app/level.h"
#include "app/snd.h"
#include "app/task.h"

/* gyrocopter.c globals (non-static there). */
extern u16 D_8036957E;                 /* the test's class */
extern Unk803599D0 D_803694D0;         /* fuel burn vs. throttle */
extern Unk802D3658_Unk111C D_80369070; /* collision capsules: [0] body, [1] rotor */

/* Rust (pw64_game::set_plane, also live from the settings screen): which
 * aircraft the gyrocopter slot flies from the next test start:
 * PW64_PLANE_OFF (the gyrocopter), PW64_PLANE_BIG (model 39, or 27 where the
 * level didn't load 39) or PW64_PLANE_SMALL (model 27). */
int pw64_plane = PW64_PLANE_OFF;
/* 1 while the current test flies the plane: latched by the first
 * gyrocopterLoadLevel of a test (replays reload the vehicle and keep it),
 * cleared by pw64_plane_end. */
int pw64_plane_run = 0;

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

#define AP_GRAVITY 9.8f
/* Lift: K * CL * airspeed^2. CL = 0.35 (wing incidence alone) holds level
 * flight at ~45 m/s; CL_MAX gives a ~22 m/s stall. */
#define AP_LIFT_K 0.0138f
#define AP_CL_SLOPE 5.0f      /* CL per unit AoA (tan) */
#define AP_ALPHA_STALL 0.24f  /* stall AoA (tan, ~16 deg) */
#define AP_CL_MAX 1.4f
#define AP_CL_FALL 6.0f       /* CL lost per unit AoA past the stall */
#define AP_CL_POST_STALL 0.6f
#define AP_WING_INCIDENCE 0.07f
#define AP_GROUND_RAKE 0.03f  /* extra AoA from the gear stance */
#define AP_ROTATE_AOA 0.14f    /* full stick back on the runway (takeoff) */
#define AP_ROTATE_SPEED 47.22f /* the elevator bites from this airspeed */
/* Drag on top of the gyro's body drag (func_80307A9C, which levels off at
 * ~7.6 m/s^2): parasitic CD0 * V^2 and induced K_I * CL * lift. Top speed
 * ~62 m/s at full throttle, ~45 m/s at the 0.5 the throttle returns to. */
#define AP_CD0 0.0015f
#define AP_INDUCED_K 0.02f
#define AP_THRUST_MAX 20.5f   /* static thrust at full throttle */
#define AP_PROP_VMAX 370.0f   /* thrust falls linearly to 0 at this airspeed */
#define AP_SIDE_DAMP 1.5f     /* fin: turns the velocity toward the nose */
#define AP_BRAKE 0.00001f         /* wheel brakes, throttle closed on the ground */
#define AP_GROUND_SPOIL 0.3f  /* lift kept on the ground, throttle closed */
/* Control: attitude hold, the stick sets bank and pitch angles. */
#define AP_MAX_BANK 0.96f     /* 55 deg */
#define AP_MAX_PITCH 0.61f    /* 35 deg */
#define AP_ROLL_GAIN 0.8f     /* rad/s at full bank error */
#define AP_PITCH_GAIN 0.5f
#define AP_CTRL_SPEED 35.0f   /* full authority from this airspeed */
#define AP_MIN_AUTHORITY 0.15f
#define AP_STALL_NOSE_DROP 0.6f /* rad/s, wing fully stalled */
#define AP_MIN_TURN_SPEED 18.0f
#define AP_MAX_TURN_RATE 0.8f

#define AP_WIND_STRENGTH 1.5f

#define AP_FUEL_DRAIN_MULTIPLIER 1.0f

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
        if ((gcData->unkA0 < 0.0f) && (airspeed > AP_ROTATE_SPEED)) {
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
    lift = AP_LIFT_K * cl * SQ(airspeed);
    /* Landing: on the wheels with the throttle closed the lift is spoiled,
     * so a touchdown sticks instead of floating off the short runways. */
    if ((gcData->unkC0 == 1) && (gcData->unkB4 < 0.05f)) {
        lift *= AP_GROUND_SPOIL;
    }
    gcData->unk31C.z += lift;
    gcData->unk334.z += lift;

    /* After a touchdown faster than 1 m/s the ground solver (func_80304B98)
     * cancels the lift for 4 s (unk8C counts up) so the gyro settles; a
     * pilot pulling back for takeoff (or a touch-and-go) ends that early. */
    if ((gcData->unkC0 == 1) && (gcData->unkA0 < 0.0f) && (airspeed > AP_ROTATE_SPEED) && (gcData->unk8C < 4.0f)) {
        gcData->unk8C = 4.0f;
    }
    drag = (AP_CD0 * SQ(airspeed)) + (AP_INDUCED_K * cl * lift);
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
        f32 throttle = gcData->unkB4;
        f32 fuelMultiplier = AP_FUEL_DRAIN_MULTIPLIER;

        if (throttle > 0.965f) {
            // Above 90%: very fast fuel consumption
            fuelMultiplier = 3.0f + (throttle - 0.5f) * 8.0f;
        } else if (throttle > 0.5f) {
            // Above 50%: moderately faster fuel consumption
            fuelMultiplier = 0.4f + (throttle - 0.5f) * 1.50f;
        } else {
            fuelMultiplier = 0.4f;
        }

        gcData->fuel -= func_80313F08(&D_803694D0, throttle) * fuelMultiplier * D_8034F854;
        //gcData->fuel -= func_80313F08(&D_803694D0, gcData->unkB4) * D_8034F854;
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
    authority = ap_clamp(airspeed / AP_CTRL_SPEED, AP_MIN_AUTHORITY, 1.0f);
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

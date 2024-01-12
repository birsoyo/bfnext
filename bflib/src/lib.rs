/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

pub mod bg;
pub mod cfg;
pub mod db;
pub mod ewr;
pub mod jtac;
pub mod menu;
pub mod msgq;
pub mod perf;
pub mod spawnctx;

extern crate nalgebra as na;
use crate::{cfg::Cfg, db::player::SlotAuth, perf::record_perf};
use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use cfg::LifeType;
use chrono::{prelude::*, Duration};
use compact_str::{format_compact, CompactString};
use db::{objective::ObjectiveId, player::RegErr, Db};
use dcso3::{
    coalition::Side,
    env::{
        self,
        miz::{Miz, UnitId},
        Env,
    },
    event::Event,
    hooks::UserHooks,
    lfs::Lfs,
    net::{Net, PlayerId, SlotId, Ucid},
    object::{DcsObject, DcsOid},
    timer::Timer,
    trigger::{MarkId, Trigger},
    unit::{ClassUnit, Unit},
    world::World,
    HooksLua, LuaEnv, MizLua, String, Vector2,
};
use ewr::Ewr;
use fxhash::{FxHashMap, FxHashSet};
use jtac::Jtacs;
use log::{debug, error, info, warn};
use mlua::prelude::*;
use msgq::MsgTyp;
use perf::Perf;
use smallvec::{smallvec, SmallVec};
use spawnctx::SpawnCtx;
use std::{iter, path::PathBuf, str::FromStr, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
enum AdminCommand {
    Help,
    ReduceInventory { airbase: String, amount: u8 },
    LogisticsTickNow,
    LogisticsDeliverNow,
    Tim { key: String, size: usize },
}

impl AdminCommand {
    fn help() -> &'static str {
        "reduce-inventory <airbase> <amount>, logistics-tick-now, logistics-deliver-now, tim <key> [size]"
    }
}

impl FromStr for AdminCommand {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s
            .strip_prefix("-admin ")
            .ok_or_else(|| anyhow!("not an admin command {s}"))?;
        if s.trim() == "help" {
            Ok(Self::Help)
        } else if s.starts_with("reduce-inventory ") {
            let s = s.strip_prefix("reduce-inventory ").unwrap();
            match s.split_once(" ") {
                None => bail!("reduce-inventory [airbase] [amount]"),
                Some((airbase, amount)) => {
                    let amount = amount.parse::<u8>()?;
                    Ok(Self::ReduceInventory {
                        airbase: String::from(airbase),
                        amount,
                    })
                }
            }
        } else if s.starts_with("logistics-tick-now") {
            Ok(Self::LogisticsTickNow)
        } else if s.starts_with("logistics-deliver-now") {
            Ok(Self::LogisticsDeliverNow)
        } else if s.starts_with("tim ") {
            let s = s.strip_prefix("tim ").unwrap();
            match s.split_once(" ") {
                None => Ok(Self::Tim {
                    key: String::from(s),
                    size: 3000,
                }),
                Some((key, size)) => {
                    let size = size.parse::<usize>()?;
                    Ok(Self::Tim {
                        key: String::from(key),
                        size,
                    })
                }
            }
        } else {
            bail!("unknown command {s}")
        }
    }
}

#[derive(Debug)]
struct PlayerInfo {
    name: String,
    ucid: Ucid,
}

#[derive(Debug, Default)]
struct Context {
    last_perf_log: DateTime<Utc>,
    loaded: bool,
    idx: env::miz::MizIndex,
    db: Db,
    admin_commands: Vec<(PlayerId, AdminCommand)>,
    to_background: Option<UnboundedSender<bg::Task>>,
    info_by_player_id: FxHashMap<PlayerId, PlayerInfo>,
    id_by_ucid: FxHashMap<Ucid, PlayerId>,
    recently_landed: FxHashMap<DcsOid<ClassUnit>, DateTime<Utc>>,
    airborne: FxHashSet<DcsOid<ClassUnit>>,
    captureable: FxHashMap<ObjectiveId, usize>,
    last_slow_timed_events: DateTime<Utc>,
    last_logistics_tick: DateTime<Utc>,
    logistics_ticks_since_delivery: u32,
    ewr: Ewr,
    jtac: Jtacs,
}

static mut CONTEXT: Option<Context> = None;

impl Context {
    // this must be used cautiously. Reasons why it's not totally nuts,
    // - the dcs scripting api is single threaded
    // - the event handlers can be triggerred by api calls, making refcells and mutexes error prone
    // - as long as an event handler doesn't step on state in an api call it's ok, since concurrency never happens
    //   that isn't so hard to guarantee
    unsafe fn get_mut() -> &'static mut Context {
        match CONTEXT.as_mut() {
            Some(ctx) => ctx,
            None => {
                CONTEXT = Some(Context::default());
                CONTEXT.as_mut().unwrap()
            }
        }
    }

    unsafe fn _get() -> &'static Context {
        Context::get_mut()
    }

    fn do_bg_task(&mut self, task: bg::Task) {
        if let Some(to_bg) = &self.to_background {
            match to_bg.send(task) {
                Ok(()) => (),
                Err(_) => panic!("background thread is dead"),
            }
        }
    }

    fn init_async_bg(&mut self, lua: &Lua) -> Result<()> {
        if self.to_background.is_none() {
            let write_dir = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str());
            self.to_background = Some(bg::init(write_dir));
        }
        Ok(())
    }

    fn respawn_groups(&mut self, lua: MizLua) -> Result<()> {
        let spctx = SpawnCtx::new(lua)?;
        self.db.respawn_after_load(&spctx)
    }

    fn log_perf(&mut self, now: DateTime<Utc>) {
        if now - self.last_perf_log > Duration::seconds(60) {
            self.last_perf_log = now;
            self.do_bg_task(bg::Task::LogPerf(Arc::clone(unsafe { Perf::get_mut() })))
        }
    }
}

fn get_player_info<'a, 'lua, L: LuaEnv<'lua>>(
    tbl: &'a mut FxHashMap<PlayerId, PlayerInfo>,
    rtbl: &'a mut FxHashMap<Ucid, PlayerId>,
    lua: L,
    id: PlayerId,
) -> Result<&'a PlayerInfo> {
    if tbl.contains_key(&id) {
        Ok(&tbl[&id])
    } else {
        let net = Net::singleton(lua)?;
        let ifo = net.get_player_info(id)?;
        let ucid = ifo
            .ucid()?
            .ok_or_else(|| anyhow!("player {:?} has no ucid", ifo))?;
        let name = ifo.name()?;
        rtbl.insert(ucid.clone(), id);
        tbl.insert(id, PlayerInfo { name, ucid });
        Ok(&tbl[&id])
    }
}

fn on_player_try_connect(
    _: HooksLua,
    addr: String,
    name: String,
    ucid: Ucid,
    id: PlayerId,
) -> Result<bool> {
    let ts = Utc::now();
    info!(
        "onPlayerTryConnect addr: {:?}, name: {:?}, ucid: {:?}, id: {:?}",
        addr, name, ucid, id
    );
    let ctx = unsafe { Context::get_mut() };
    ctx.id_by_ucid.insert(ucid.clone(), id);
    ctx.info_by_player_id.insert(id, PlayerInfo { name, ucid });
    record_perf(&mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_hooks, ts);
    Ok(true)
}

fn register_player(lua: HooksLua, id: PlayerId, msg: String) -> Result<String> {
    let ctx = unsafe { Context::get_mut() };
    let ifo = get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id)?;
    let side = if msg.eq_ignore_ascii_case("blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("red") {
        Side::Red
    } else {
        bail!("side \"{msg}\" is not blue or red")
    };
    match ctx
        .db
        .register_player(ifo.ucid.clone(), ifo.name.clone(), side)
    {
        Ok(()) => {
            let msg = String::from(format_compact!("Welcome to the {:?} team. You may only occupy slots belonging to your team. Good luck!", side));
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(None),
                format_compact!("{} has joined {:?} team", ifo.name, side),
            );
        }
        Err(RegErr::AlreadyOn(side)) => ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            format_compact!("you are already on {:?} team!", side),
        ),
        Err(RegErr::AlreadyRegistered(side_switches, orig_side)) => {
            let msg = String::from(match side_switches {
                None => format_compact!("You are already on the {:?} team. You may switch sides by typing -switch {:?}.", orig_side, side),
                Some(0) => format_compact!("You are already on {:?} team, and you may not switch sides.", orig_side),
                Some(1) => format_compact!("You are already on {:?} team. You may sitch sides 1 time by typing -switch {:?}.", orig_side, side),
                Some(n) => format_compact!("You are already on {:?} team. You may switch sides {n} times. Type -switch {:?}.", orig_side, side),
            });
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
    }
    Ok("".into())
}

fn sideswitch_player(lua: HooksLua, id: PlayerId, msg: String) -> Result<String> {
    let ctx = unsafe { Context::get_mut() };
    let ifo = get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id)?;
    let (_, slot) = Net::singleton(lua)?.get_slot(id)?;
    if !slot.is_spectator() {
        bail!("you must be in spectators to switch sides")
    }
    let side = if msg.eq_ignore_ascii_case("-switch blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("-switch red") {
        Side::Red
    } else {
        bail!("side must be blue or red \"{msg}\"");
    };
    match ctx.db.sideswitch_player(&ifo.ucid, side) {
        Ok(()) => {
            let msg = String::from(format_compact!("{} has switched to {:?}", ifo.name, side));
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(None), msg);
        }
        Err(e) => ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), e),
    }
    Ok("".into())
}

fn lives_command(id: PlayerId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let ifo = ctx
        .info_by_player_id
        .get(&id)
        .ok_or_else(|| anyhow!("missing info for player {:?}", id))?;
    let msg = lives(&mut ctx.db, &ifo.ucid, None)?;
    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
    Ok(())
}

fn do_admin_command(id: PlayerId, cmd: String) {
    let ctx = unsafe { Context::get_mut() };
    let ifo = match ctx.info_by_player_id.get(&id) {
        Some(ifo) => ifo,
        None => return,
    };
    if !ctx.db.ephemeral.cfg().admins.contains(&ifo.ucid) {
        return;
    }
    match cmd.parse::<AdminCommand>() {
        Err(e) => ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            format_compact!("parse error {:?}", e),
        ),
        Ok(AdminCommand::Help) => {
            ctx.db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), AdminCommand::help());
        }
        Ok(cmd) => {
            info!("queueing admin command {:?} from {:?}", cmd, ifo);
            ctx.admin_commands.push((id, cmd))
        }
    }
}

fn on_player_try_send_chat(lua: HooksLua, id: PlayerId, msg: String, all: bool) -> Result<String> {
    let start_ts = Utc::now();
    info!(
        "onPlayerTrySendChat id: {:?}, msg: {:?}, all: {:?}",
        id, msg, all
    );
    let r = if msg.eq_ignore_ascii_case("blue") || msg.eq_ignore_ascii_case("red") {
        register_player(lua, id, msg)
    } else if msg.eq_ignore_ascii_case("-switch blue") || msg.eq_ignore_ascii_case("-switch red") {
        sideswitch_player(lua, id, msg)
    } else if msg.eq_ignore_ascii_case("-lives") {
        if let Err(e) = lives_command(id) {
            error!("lives command failed for player {:?} {:?}", id, e);
        }
        record_perf(
            &mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_hooks,
            start_ts,
        );
        Ok("".into())
    } else if msg.starts_with("-admin ") {
        do_admin_command(id, msg);
        Ok("".into())
    } else {
        record_perf(
            &mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_hooks,
            start_ts,
        );
        Ok(msg)
    };
    match r {
        Ok(s) => Ok(s),
        Err(e) => {
            unsafe { Context::get_mut() }
                .db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), format_compact!("{e}"));
            Ok("".into())
        }
    }
}

fn try_occupy_slot(id: PlayerId, ifo: &PlayerInfo, side: Side, slot: SlotId) -> Result<bool> {
    let now = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    match ctx.db.try_occupy_slot(now, side, slot, &ifo.ucid) {
        SlotAuth::NoLives => Ok(false),
        SlotAuth::ObjectiveHasNoLogistics => {
            let msg = format_compact!("Objective is capturable");
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
            Ok(false)
        }
        SlotAuth::NotRegistered(side) => {
            let msg = String::from(format_compact!(
                "You must join {:?} to use this slot. Type {:?} in chat.",
                side,
                side
            ));
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
            Ok(false)
        }
        SlotAuth::ObjectiveNotOwned(side) => {
            let msg = String::from(format_compact!(
                "{:?} does not own the objective associated with this slot",
                side
            ));
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
            Ok(false)
        }
        SlotAuth::Yes => Ok(true),
    }
}

fn on_player_try_change_slot(
    lua: HooksLua,
    id: PlayerId,
    side: Side,
    slot: SlotId,
) -> Result<Option<bool>> {
    info!("onPlayerTryChangeSlot: {:?}", id);
    if slot.is_spectator() {
        return Ok(None);
    }
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let res = match get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id) {
        Err(e) => {
            error!("failed to get player info for {:?} {:?}", id, e);
            Ok(Some(false))
        }
        Ok(ifo) => match try_occupy_slot(id, &ifo, side, slot) {
            Err(e) => {
                error!("error checking slot {:?}", e);
                Ok(Some(false))
            }
            Ok(false) => Ok(Some(false)),
            Ok(true) => Ok(None),
        },
    };
    record_perf(
        &mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_hooks,
        start_ts,
    );
    res
}

fn unit_killed(lua: MizLua, ctx: &mut Context, id: DcsOid<ClassUnit>) -> Result<()> {
    ctx.recently_landed.remove(&id);
    if let Err(e) = ctx.jtac.unit_dead(lua, &mut ctx.db, &id) {
        error!("jtac unit dead failed for {:?} {:?}", id, e)
    }
    if let Err(e) = ctx.db.unit_dead(&id, Utc::now()) {
        error!("unit dead failed for {:?} {:?}", id, e);
    }
    Ok(())
}

fn on_event(lua: MizLua, ev: Event) -> Result<()> {
    let start_ts = Utc::now();
    info!("onEvent: {:?}", ev);
    let ctx = unsafe { Context::get_mut() };
    match ev {
        Event::Birth(b) => {
            if let Ok(unit) = b.initiator.as_unit() {
                if let Err(e) = ctx.db.unit_born(&unit) {
                    error!("unit born failed {:?} {:?}", unit, e);
                }
            }
        }
        Event::PlayerEnterUnit(e) => {
            if let Some(o) = &e.initiator {
                if let Ok(unit) = o.as_unit() {
                    if let Err(e) = ctx.db.player_entered_unit(&unit) {
                        error!("player enter unit failed {:?} {:?}", unit, e)
                    }
                }
            }
        }
        Event::PlayerLeaveUnit(e) => {
            if let Some(o) = &e.initiator {
                if let Ok(unit) = o.as_unit() {
                    if let Err(e) = ctx.db.player_left_unit(lua, &unit) {
                        error!("player left unit failed {:?} {:?}", unit, e)
                    }
                }
            }
        }
        Event::Dead(e) | Event::UnitLost(e) | Event::PilotDead(e) => {
            if let Some(unit) = e.initiator {
                if let Ok(unit) = unit.as_unit() {
                    if let Err(e) = unit_killed(lua, ctx, unit.object_id()?) {
                        error!("unit killed failed {}", e)
                    }
                }
            }
        }
        Event::Ejection(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                if let Err(e) = unit_killed(lua, ctx, unit.object_id()?) {
                    error!("unit killed failed {}", e)
                }
            }
        }
        Event::Takeoff(e) | Event::PostponedTakeoff(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let id = unit.object_id()?;
                let slot = unit.slot()?;
                let ctx = unsafe { Context::get_mut() };
                if ctx.airborne.insert(id.clone()) && ctx.recently_landed.remove(&id).is_none() {
                    let pos = unit.get_point()?;
                    match ctx
                        .db
                        .takeoff(Utc::now(), slot.clone(), Vector2::new(pos.x, pos.z))
                    {
                        Err(e) => error!("could not process takeoff, {:?}", e),
                        Ok(None) => (),
                        Ok(Some(typ)) => {
                            if let Err(e) = message_life(ctx, &slot, Some(typ), "life taken\n") {
                                error!("could not display life taken message {:?}", e)
                            }
                            let _ = menu::list_cargo_for_slot(lua, ctx, &slot);
                        }
                    }
                }
            }
        }
        Event::Land(e) | Event::PostponedLand(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let id = unit.object_id()?;
                let ctx = unsafe { Context::get_mut() };
                if ctx.airborne.remove(&id) {
                    ctx.recently_landed.insert(id, Utc::now());
                }
            }
        }
        Event::MissionEnd => unsafe {
            CONTEXT = None;
            Context::get_mut().init_async_bg(lua.inner())?;
        },
        _ => (),
    }
    record_perf(
        &mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_events,
        start_ts,
    );
    Ok(())
}

fn lives(db: &mut Db, ucid: &Ucid, typfilter: Option<LifeType>) -> Result<CompactString> {
    db.maybe_reset_lives(ucid, Utc::now())?;
    let player = db
        .player(ucid)
        .ok_or_else(|| anyhow!("no such player {:?}", ucid))?;
    let cfg = db.ephemeral.cfg();
    let lives = &player.lives;
    let mut msg = CompactString::new("");
    let now = Utc::now();
    for (typ, (n, reset_after)) in &cfg.default_lives {
        if typfilter.is_none() || Some(*typ) == typfilter {
            match lives.get(typ) {
                None => msg.push_str(&format_compact!("{typ} {n}/{n}\n")),
                Some((reset, cur)) => {
                    let since_reset = now - *reset;
                    let reset = Duration::seconds(*reset_after as i64) - since_reset;
                    let hrs = reset.num_hours();
                    let min = reset.num_minutes() - hrs * 60;
                    let sec = reset.num_seconds() - hrs * 3600 - min * 60;
                    msg.push_str(&format_compact!(
                        "{typ} {cur}/{n} resetting in {:02}:{:02}:{:02}\n",
                        hrs,
                        min,
                        sec
                    ));
                }
            }
        }
    }
    Ok(msg)
}

fn message_life(ctx: &mut Context, slot: &SlotId, typ: Option<LifeType>, msg: &str) -> Result<()> {
    let uid = slot.as_unit_id().ok_or_else(|| anyhow!("not a unit"))?;
    let ucid = ctx
        .db
        .ephemeral
        .player_in_slot(slot)
        .ok_or_else(|| anyhow!("no player in slot {:?}", slot))?
        .clone();
    let mut msg = CompactString::new(msg);
    if let Ok(lives) = lives(&mut ctx.db, &ucid, typ) {
        msg.push_str(&lives)
    }
    ctx.db.ephemeral.msgs().panel_to_unit(10, false, uid, msg);
    Ok(())
}

fn run_admin_commands(ctx: &mut Context, lua: MizLua) -> Result<()> {
    use std::fmt::Write;
    for (id, cmd) in ctx.admin_commands.drain(..) {
        match cmd {
            AdminCommand::Help => (),
            AdminCommand::ReduceInventory { airbase, amount } => {
                match ctx.db.admin_reduce_inventory(lua, airbase.as_str(), amount) {
                    Err(e) => ctx.db.ephemeral.msgs().send(
                        MsgTyp::Chat(Some(id)),
                        format_compact!("reduce inventory failed: {:?}", e),
                    ),
                    Ok(()) => ctx
                        .db
                        .ephemeral
                        .msgs()
                        .send(MsgTyp::Chat(Some(id)), "inventory reduced"),
                }
            }
            AdminCommand::LogisticsTickNow => {
                let mut msg = CompactString::new("");
                if let Err(e) = ctx.db.sync_objectives_from_warehouses(lua) {
                    write!(msg, "failed to sync objectives from warehouses {:?} ", e)?
                }
                if let Err(e) = ctx.db.deliver_supplies_from_logistics_hubs() {
                    write!(msg, "failed to deliver supplies from hubs {:?} ", e)?
                }
                if let Err(e) = ctx.db.sync_warehouses_from_objectives(lua) {
                    write!(msg, "failed to sync warehouses from objectives {:?}", e)?
                }
                if msg.is_empty() {
                    ctx.db
                        .ephemeral
                        .msgs()
                        .send(MsgTyp::Chat(Some(id)), "tick complete")
                } else {
                    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg)
                }
            }
            AdminCommand::LogisticsDeliverNow => {
                let mut msg = CompactString::new("");
                if let Err(e) = ctx.db.sync_objectives_from_warehouses(lua) {
                    write!(msg, "failed to sync objectives from warehouses {:?} ", e)?
                }
                if let Err(e) = ctx.db.deliver_production(lua) {
                    error!("failed to deliver production {:?}", e)
                }
                if let Err(e) = ctx.db.sync_warehouses_from_objectives(lua) {
                    write!(msg, "failed to sync warehouses from objectives {:?}", e)?
                }
                if msg.is_empty() {
                    ctx.db
                        .ephemeral
                        .msgs()
                        .send(MsgTyp::Chat(Some(id)), "deliver complete")
                } else {
                    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg)
                }
            }
            AdminCommand::Tim { key, size } => {
                let mut to_remove: SmallVec<[MarkId; 8]> = smallvec![];
                let act = Trigger::singleton(lua)?.action()?;
                for mk in World::singleton(lua)?
                    .get_mark_panels()
                    .context("getting marks")?
                {
                    let mk = mk?;
                    if mk.text == key {
                        to_remove.push(mk.id);
                        act.explosion(mk.pos, size as f32).context("making boom")?;
                    }
                }
                for id in to_remove {
                    ctx.db.ephemeral.msgs().delete_mark(id);
                }
            }
        }
    }
    Ok(())
}

fn return_lives(lua: MizLua, ctx: &mut Context, ts: DateTime<Utc>) {
    macro_rules! or_false {
        ($e:expr) => {
            match $e {
                Ok(r) => r,
                Err(_) => return false,
            }
        };
    }
    let db = &mut ctx.db;
    let mut returned: SmallVec<[(LifeType, SlotId); 4]> = smallvec![];
    ctx.recently_landed.retain(|id, landed_ts| {
        if ts - *landed_ts >= Duration::seconds(10) {
            let unit = or_false!(Unit::get_instance(lua, id));
            let pos = or_false!(unit.get_ground_position());
            let slot = or_false!(unit.slot());
            if let Some(typ) = db.land(slot.clone(), pos.0) {
                returned.push((typ, slot));
                return false;
            }
        }
        true
    });
    for (typ, slot) in returned {
        if let Err(e) = message_life(ctx, &slot, Some(typ), "life returned\n") {
            error!("failed to send life returned message to {:?} {}", slot, e);
        }
    }
}

fn advise_captureable(ctx: &mut Context) -> Result<()> {
    let cur_cap = ctx.db.capturable_objectives();
    for oid in &cur_cap {
        let dur = ctx.captureable.entry(*oid).or_default();
        *dur += 1;
        if *dur == 10 {
            let m = format_compact!("{} is now capturable", ctx.db.objective(oid)?.name());
            ctx.db.ephemeral.msgs().panel_to_all(30, false, m);
        }
    }
    ctx.captureable.retain(|oid, _| cur_cap.contains(oid));
    Ok(())
}

fn advise_captured(ctx: &mut Context, ts: DateTime<Utc>) -> Result<()> {
    for (side, oid) in ctx.db.check_capture(ts)? {
        let name = ctx.db.objective(&oid)?.name();
        let mcap = format_compact!("our forces have captured {}", name);
        let mlost = format_compact!("we have lost {}", name);
        ctx.db.ephemeral.msgs().panel_to_side(15, false, side, mcap);
        ctx.db
            .ephemeral
            .msgs()
            .panel_to_side(15, false, side.opposite(), mlost);
        ctx.captureable.remove(&oid);
    }
    Ok(())
}

fn generate_ewr_reports(ctx: &mut Context, now: DateTime<Utc>) -> Result<()> {
    use std::fmt::Write;
    let mut msgs: SmallVec<[(UnitId, CompactString); 64]> = smallvec![];
    for (ucid, player, inst) in ctx.db.instanced_players() {
        let uid = match player
            .current_slot
            .as_ref()
            .and_then(|(sl, _)| sl.as_unit_id())
        {
            Some(uid) => uid,
            None => continue,
        };
        let braa_to_chickens = ctx.ewr.where_chicken(now, false, false, ucid, player, inst);
        if !braa_to_chickens.is_empty() {
            let mut report = format_compact!("Bandits BRAA\n");
            write!(report, "{}\n", ewr::HEADER)?;
            for gibbraa in braa_to_chickens {
                write!(report, "{gibbraa}\n")?;
            }
            msgs.push((uid, report));
        }
    }
    for (uid, msg) in msgs {
        ctx.db.ephemeral.msgs().panel_to_unit(10, false, uid, msg)
    }
    Ok(())
}

fn run_logistics_events(
    lua: MizLua,
    ctx: &mut Context,
    perf: &mut Perf,
    ts: DateTime<Utc>,
) -> Result<()> {
    if let Some(wcfg) = ctx.db.ephemeral.cfg().warehouse.as_ref() {
        let freq = Duration::minutes(wcfg.tick as i64);
        let ticks_per_delivery = wcfg.ticks_per_delivery;
        if ts - ctx.last_logistics_tick >= freq {
            ctx.last_logistics_tick = ts;
            let start_ts = Utc::now();
            if let Err(e) = ctx.db.sync_objectives_from_warehouses(lua) {
                error!("failed to sync objectives from warehouses {:?}", e)
            }
            if ctx.logistics_ticks_since_delivery >= ticks_per_delivery {
                ctx.logistics_ticks_since_delivery = 0;
                if let Err(e) = ctx.db.deliver_production(lua) {
                    error!("failed to deliver production {:?}", e)
                }
            } else {
                ctx.logistics_ticks_since_delivery += 1;
                if let Err(e) = ctx.db.deliver_supplies_from_logistics_hubs() {
                    error!("failed to deliver supplies from hubs {:?}", e)
                }
            }
            if let Err(e) = ctx.db.sync_warehouses_from_objectives(lua) {
                error!("failed to sync warehouses from objectives {:?}", e)
            }
            record_perf(&mut perf.logistics, start_ts);
        }
    }
    Ok(())
}

fn run_slow_timed_events(
    lua: MizLua,
    ctx: &mut Context,
    perf: &mut Perf,
    ts: DateTime<Utc>,
) -> Result<()> {
    let freq = Duration::seconds(ctx.db.ephemeral.cfg().slow_timed_events_freq as i64);
    if ts - ctx.last_slow_timed_events >= freq {
        ctx.last_slow_timed_events = ts;
        let start_ts = Utc::now();
        let mut dead = vec![];
        match ctx.db.update_unit_positions::<iter::Once<_>>(lua, None) {
            Err(e) => error!("could not update unit positions {e}"),
            Ok(v) => dead = v,
        }
        record_perf(&mut perf.unit_positions, start_ts);
        let ts = Utc::now();
        match ctx.db.update_player_positions(lua) {
            Err(e) => error!("could not update player positions {e}"),
            Ok(mut v) => dead.extend(v.drain(..)),
        }
        for id in dead {
            if let Err(e) = unit_killed(lua, ctx, id.clone()) {
                error!("unit killed failed {:?} {:?}", id, e)
            }
        }
        record_perf(&mut perf.player_positions, ts);
        let ts = Utc::now();
        if let Err(e) = ctx.ewr.update_tracks(lua, &ctx.db, ts) {
            error!("could not update ewr tracks {e}")
        }
        record_perf(&mut perf.ewr_tracks, ts);
        let ts = Utc::now();
        if let Err(e) = generate_ewr_reports(ctx, ts) {
            error!("could not generate ewr reports {e}")
        }
        record_perf(&mut perf.ewr_reports, ts);
        let ts = Utc::now();
        match ctx.db.cull_or_respawn_objectives(lua, ts) {
            Err(e) => error!("could not cull or respawn objectives {e}"),
            Ok((threatened, cleared)) => {
                for oid in threatened {
                    let obj = ctx.db.objective(&oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!("enemies spotted near {}", obj.name());
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
                for oid in cleared {
                    let obj = ctx.db.objective(&oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!("{} is no longer threatened", obj.name());
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
            }
        }
        record_perf(&mut perf.unit_culling, ts);
        let ts = Utc::now();
        if let Err(e) = ctx.db.update_objectives_markup() {
            error!("could not remark objectives {e}")
        }
        record_perf(&mut perf.remark_objectives, ts);
        let ts = Utc::now();
        if let Err(e) = ctx.jtac.update_contacts(lua, &mut ctx.db) {
            error!("could not update jtac contacts {e}")
        }
        record_perf(&mut perf.update_jtac_contacts, ts);
        record_perf(&mut perf.slow_timed, start_ts);
    }
    Ok(())
}

fn run_timed_events(lua: MizLua, path: &PathBuf) -> Result<()> {
    let ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let perf = Arc::make_mut(unsafe { Perf::get_mut() });
    if let Err(e) = ctx.db.maybe_do_repairs(ts) {
        error!("error doing repairs {:?}", e)
    }
    record_perf(&mut perf.do_repairs, ts);
    return_lives(lua, ctx, ts);
    let net = Net::singleton(lua)?;
    let act = Trigger::singleton(lua)?.action()?;
    for ucid in ctx.db.ephemeral.players_to_force_to_spectators() {
        match ctx.id_by_ucid.get(&ucid) {
            None => warn!("no id for player ucid {:?}", ucid),
            Some(id) => {
                if let Err(e) = net.force_player_slot(*id, Side::Neutral, SlotId::spectator()) {
                    error!("error forcing player {:?} to spectators {:?}", id, e);
                }
            }
        }
    }
    if let Err(e) = run_slow_timed_events(lua, ctx, perf, ts) {
        error!("error running slow timed events {:?}", e)
    }
    let now = Utc::now();
    let spctx = SpawnCtx::new(lua)?;
    if let Err(e) = ctx
        .db
        .ephemeral
        .process_spawn_queue(&ctx.db.persisted, ts, &ctx.idx, &spctx)
    {
        error!("error processing spawn queue {:?}", e)
    }
    record_perf(&mut perf.spawn_queue, now);
    let now = Utc::now();
    if let Err(e) = advise_captured(ctx, ts) {
        error!("error advise captured {:?}", e)
    }
    record_perf(&mut perf.advise_captured, now);
    let now = Utc::now();
    if let Err(e) = advise_captureable(ctx) {
        error!("error advise capturable {:?}", e)
    }
    record_perf(&mut perf.advise_capturable, now);
    let now = Utc::now();
    match ctx.jtac.update_target_positions(lua, &mut ctx.db) {
        Err(e) => error!("error updating jtac target positions {:?}", e),
        Ok(dead) => {
            for id in dead {
                if let Err(e) = unit_killed(lua, ctx, id.clone()) {
                    error!("unit killed failed {:?} {:?}", id, e)
                }
            }
        }
    }
    record_perf(&mut perf.jtac_target_positions, now);
    let now = Utc::now();
    ctx.db.ephemeral.msgs().process(&net, &act);
    record_perf(&mut perf.process_messages, now);
    let now = Utc::now();
    if let Some(snap) = ctx.db.maybe_snapshot() {
        ctx.do_bg_task(bg::Task::SaveState(path.clone(), snap));
    }
    record_perf(&mut perf.snapshot, now);
    if let Err(e) = run_logistics_events(lua, ctx, perf, ts) {
        error!("error running logistics events {:?}", e)
    }
    if let Err(e) = run_admin_commands(ctx, lua) {
        error!("failed to run admin commands {:?}", e)
    }
    record_perf(&mut perf.timed_events, ts);
    ctx.log_perf(now);
    Ok(())
}

fn start_timed_events(lua: MizLua, path: PathBuf) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    timer.schedule_function(timer.get_time()? + 1., mlua::Value::Nil, {
        let path = path.clone();
        move |lua, _, now| {
            if let Err(e) = run_timed_events(lua, &path) {
                error!("failed to run timed events {:?}", e)
            }
            Ok(Some(now + 1.))
        }
    })?;
    Ok(())
}

fn delayed_init_miz(lua: MizLua) -> Result<()> {
    info!("init_miz");
    let ctx = unsafe { Context::get_mut() };
    info!("indexing the miz");
    let miz = Miz::singleton(lua)?;
    ctx.idx = miz.index().context("indexing the mission")?;
    info!("adding event handlers");
    World::singleton(lua)?
        .add_event_handler(on_event)
        .context("adding event handlers")?;
    let sortie = miz.sortie().context("getting the sortie")?;
    debug!("sortie is {:?}", sortie);
    let path = match Env::singleton(lua)?.get_value_dict_by_key(sortie)?.as_str() {
        "" => bail!("missing sortie in miz file"),
        s => PathBuf::from(format_compact!("{}\\{}", Lfs::singleton(lua)?.writedir()?, s).as_str()),
    };
    debug!("path to saved state is {:?}", path);
    info!("initializing db");
    if !path.exists() {
        debug!("saved state doesn't exist, starting from default");
        let cfg = Cfg::load(&path)?;
        ctx.db = Db::init(lua, cfg, &ctx.idx, &miz).context("initalizing the mission")?;
    } else {
        debug!("saved state exists, loading it");
        ctx.db = Db::load(&miz, &ctx.idx, &path).context("loading the saved state")?;
    }
    info!("spawning units");
    ctx.respawn_groups(lua)
        .context("setting up the mission after load")?;
    info!("initializing menus");
    menu::init(&ctx, lua).context("initalizing the menus")?;
    info!("starting timed events");
    start_timed_events(lua, path).context("starting the timed events loop")?;
    Ok(())
}

fn on_mission_load_end(_lua: HooksLua) -> Result<()> {
    unsafe { Context::get_mut().loaded = true };
    debug!("mission loaded");
    Ok(())
}

fn on_player_disconnect(_: HooksLua, id: PlayerId) -> Result<()> {
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    if let Some(ifo) = ctx.info_by_player_id.remove(&id) {
        ctx.db.player_deslot(&ifo.ucid)
    }
    record_perf(
        &mut Arc::make_mut(unsafe { Perf::get_mut() }).dcs_hooks,
        start_ts,
    );
    Ok(())
}

fn init_hooks(lua: HooksLua) -> Result<()> {
    info!("setting user hooks");
    UserHooks::new(lua)
        .on_player_try_change_slot(on_player_try_change_slot)?
        .on_mission_load_end(on_mission_load_end)?
        .on_player_try_connect(on_player_try_connect)?
        .on_player_try_send_chat(on_player_try_send_chat)?
        .on_player_disconnect(on_player_disconnect)?
        .register()?;
    Ok(())
}

fn init_miz(lua: MizLua) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + 1.;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, now| {
        let ctx = unsafe { Context::get_mut() };
        if ctx.loaded {
            if let Err(e) = delayed_init_miz(lua) {
                error!("THE MISSION CANNOT START: {:?}", e);
                let timer = Timer::singleton(lua)?;
                timer.schedule_function(now + 1., mlua::Value::Nil, move |lua, _, now| {
                    let _ = Trigger::singleton(lua)?.action()?.out_text(
                        format_compact!("THE MISSION CANNOT START BECAUSE OF AN ERROR\n\n{:?}", e)
                            .into(),
                        3600,
                        true,
                    );
                    Ok(Some(now + 10.))
                })?;
            }
            Ok(None)
        } else {
            Ok(Some(now + 1.))
        }
    })?;
    Ok(())
}

#[mlua::lua_module]
fn bflib(lua: &Lua) -> LuaResult<LuaTable> {
    unsafe { Context::get_mut() }
        .init_async_bg(lua.inner())
        .map_err(dcso3::lua_err)?;
    dcso3::create_root_module(lua, init_hooks, init_miz)
}

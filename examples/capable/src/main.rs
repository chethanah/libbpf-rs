// SPDX-License-Identifier: (LGPL-2.1 OR BSD-2-Clause)
// Copyright (c) 2021 BMC Software, Inc.
// Author Devasia Thomas <https://www.linkedin.com/in/devasiathomas/>
//
// Based on capable(8) by Brendan Gregg
use core::time::Duration;
use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::Local;
use libbpf_rs::PerfBufferBuilder;
use phf::phf_map;
use plain::Plain;
use structopt::StructOpt;
use libbpf_sys;
use libc::c_int;
use libc::c_void;
use std::path::Path;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::fs::OpenOptions;
use std::fs;
use std::io::Write;

#[path = "bpf/.output/capable.skel.rs"]
mod capable;

use capable::capable_rodata_types::uniqueness;
use capable::*;

static CAPS: phf::Map<i32, &'static str> = phf_map! {
    0i32 => "CAP_CHOWN",
    1i32 => "CAP_DAC_OVERRIDE",
    2i32 => "CAP_DAC_READ_SEARCH",
    3i32 => "CAP_FOWNER",
    4i32 => "CAP_FSETID",
    5i32 => "CAP_KILL",
    6i32 => "CAP_SETGID",
    7i32 => "CAP_SETUID",
    8i32 => "CAP_SETPCAP",
    9i32 => "CAP_LINUX_IMMUTABLE",
    10i32 => "CAP_NET_BIND_SERVICE",
    11i32 => "CAP_NET_BROADCAST",
    12i32 => "CAP_NET_ADMIN",
    13i32 => "CAP_NET_RAW",
    14i32 => "CAP_IPC_LOCK",
    15i32 => "CAP_IPC_OWNER",
    16i32 => "CAP_SYS_MODULE",
    17i32 => "CAP_SYS_RAWIO",
    18i32 => "CAP_SYS_CHROOT",
    19i32 => "CAP_SYS_PTRACE",
    20i32 => "CAP_SYS_PACCT",
    21i32 => "CAP_SYS_ADMIN",
    22i32 => "CAP_SYS_BOOT",
    23i32 => "CAP_SYS_NICE",
    24i32 => "CAP_SYS_RESOURCE",
    25i32 => "CAP_SYS_TIME",
    26i32 => "CAP_SYS_TTY_CONFIG",
    27i32 => "CAP_MKNOD",
    28i32 => "CAP_LEASE",
    29i32 => "CAP_AUDIT_WRITE",
    30i32 => "CAP_AUDIT_CONTROL",
    31i32 => "CAP_SETFCAP",
    32i32 => "CAP_MAC_OVERRIDE",
    33i32 => "CAP_MAC_ADMIN",
    34i32 => "CAP_SYSLOG",
    35i32 => "CAP_WAKE_ALARM",
    36i32 => "CAP_BLOCK_SUSPEND",
    37i32 => "CAP_AUDIT_READ",
    38i32 => "CAP_PERFMON",
    39i32 => "CAP_BPF",
    40i32 => "CAP_CHECKPOINT_RESTORE",
};

impl FromStr for uniqueness {
    type Err = &'static str;
    fn from_str(unq_type: &str) -> Result<Self, Self::Err> {
        let unq_type_lower: &str = &unq_type.to_lowercase();
        match unq_type_lower {
            "off" => Ok(uniqueness::UNQ_OFF),
            "pid" => Ok(uniqueness::UNQ_PID),
            "cgroup" => Ok(uniqueness::UNQ_CGROUP),
            _ => Err("Use 1 for pid (default), 2 for cgroups"),
        }
    }
}

/// Trace capabilities
#[derive(Debug, Clone, StructOpt)]
#[structopt(name = "examples", about = "Usage instructions")]
struct Command {
    /// verbose: include non-audit checks
    #[structopt(short, long)]
    verbose: bool,
    /// only trace <pid>
    #[structopt(short, long, default_value = "0")]
    pid: u32,
    /// extra fields: Show TID and INSETID columns
    #[structopt(short = "x", long = "extra")]
    extra_fields: bool,
    /// don't repeat same info for the same <pid> or <cgroup>
    #[structopt(long = "unique", default_value = "off")]
    unique_type: uniqueness,
    /// debug output for libbpf-rs
    #[structopt(long)]
    debug: bool,
    /// trace cgroup path
    #[structopt(long, short)]
    cgroups_path: String,
    /// output file name
    #[structopt(short, long, default_value = "/tmp/bpf_capable.log")]
    output_file: String,
}

unsafe impl Plain for capable_bss_types::event {}

fn bump_memlock_rlimit() -> Result<()> {
    let rlimit = libc::rlimit {
        rlim_cur: 128 << 20,
        rlim_max: 128 << 20,
    };

    if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlimit) } != 0 {
        bail!("Failed to increase rlimit");
    }

    Ok(())
}

fn print_banner(extra_fields: bool) {
    if extra_fields {
        println!(
            "{:9} {:6} {:6} {:6} {:16} {:4} {:20} {:6} {}",
            "TIME", "UID", "PID", "TID", "COMM", "CAP", "NAME", "AUDIT", "INSETID"
        );
    } else {
        println!(
            "{:9} {:6} {:6} {:16} {:4} {:20} {:6}",
            "TIME", "UID", "PID", "COMM", "CAP", "NAME", "AUDIT"
        );
    }
}

fn handle_lost_events(cpu: i32, count: u64) {
    eprintln!("Lost {} events on CPU {}", count, cpu);
}

fn main() -> Result<()> {
    let opts = Command::from_args();

    let mut skel_builder = CapableSkelBuilder::default();
    if opts.debug {
        skel_builder.obj_builder.debug(true);
    }

    bump_memlock_rlimit()?;

    // Open
    let mut open_skel = skel_builder.open()?;
    //Pass configuration to BPF
    open_skel.rodata().tool_config.tgid = opts.pid; //tgid in kernel is pid in userland
    open_skel.rodata().tool_config.verbose = opts.verbose;
    open_skel.rodata().tool_config.unique_type = opts.unique_type;

    // load
    let mut skel = open_skel.load()?;

    let mut idx = 0;
    let path = Path::new(&opts.cgroups_path);
    let file = match File::open(&path) {
        Err(_) => panic!("Unable to open cgroupsPath"),
        Ok(file) => file,
    };
    unsafe {
        let cgroupfd= &mut file.as_raw_fd() as *mut c_int as *mut c_void;
        let index = &mut idx as *mut c_int as *mut c_void;
        libbpf_sys::bpf_map_update_elem(skel.maps_mut().cgroup_map().fd(), index, cgroupfd, libbpf_sys::BPF_ANY.into());
    }

    // attach
    skel.attach()?;

    if Path::new(&opts.output_file).exists() {
        fs::remove_file(&opts.output_file).unwrap();
    }

    print_banner(opts.extra_fields);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&opts.output_file)
        .unwrap();
    if opts.extra_fields {
        if let Err(e) = writeln!(
            file,
            "{:9} {:6} {:6} {:6} {:16} {:4} {:20} {:6} {}",
            "TIME", "UID", "PID", "TID", "COMM", "CAP", "NAME", "AUDIT", "INSETID"
        ) {
            eprintln!("Couldn't write to file: {}", e);
        }
    } else {
        if let Err(e) = writeln!(
            file,
            "{:9} {:6} {:6} {:16} {:4} {:20} {:6}",
            "TIME", "UID", "PID", "COMM", "CAP", "NAME", "AUDIT"
        ) {
            eprintln!("Couldn't write to file: {}", e);
        }
    }

    let handle_event = move |_cpu: i32, data: &[u8]| {
        let mut event = capable_bss_types::event::default();
        plain::copy_from_bytes(&mut event, data).expect("Data buffer was too short");
        let now = Local::now().format("%H:%M:%S");
        let comm_str = std::str::from_utf8(&event.comm)
            .unwrap()
            .trim_end_matches(char::from(0));
        let cap_name = match CAPS.get(&event.cap) {
            Some(&x) => x,
            None => "?",
        };
        if opts.extra_fields {
            if let Err(e) = writeln!(
                file,
                "{:9} {:6} {:<6} {:<6} {:<16} {:<4} {:<20} {:<6} {}",
                now,
                event.uid,
                event.tgid,
                event.pid,
                comm_str,
                event.cap,
                cap_name,
                event.audit,
                event.insetid
            ) {
                eprintln!("Couldn't write to file: {}", e);
            }
            println!(
                "{:9} {:6} {:<6} {:<6} {:<16} {:<4} {:<20} {:<6} {}",
                now,
                event.uid,
                event.tgid,
                event.pid,
                comm_str,
                event.cap,
                cap_name,
                event.audit,
                event.insetid
            );
        } else {
            if let Err(e) = writeln!(
                file,
                "{:9} {:6} {:<6} {:<16} {:<4} {:<20} {:<6}",
                now, event.uid, event.tgid, comm_str, event.cap, cap_name, event.audit
            ) {
                eprintln!("Couldn't write to file: {}", e);
            }
            println!(
                "{:9} {:6} {:<6} {:<16} {:<4} {:<20} {:<6}",
                now, event.uid, event.tgid, comm_str, event.cap, cap_name, event.audit
            );
        }
    };
    let perf = PerfBufferBuilder::new(skel.maps_mut().events())
        .sample_cb(handle_event)
        .lost_cb(handle_lost_events)
        .build()?;

    loop {
        perf.poll(Duration::from_millis(100))?;
    }
}

// SPDX-License-Identifier: GPL-2.0
// Copyright (c) 2021 BMC Software, Inc.
// Author Devasia Thomas <https://www.linkedin.com/in/devasiathomas/>

#include "vmlinux.h"
#include "capable.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

extern int LINUX_KERNEL_VERSION __kconfig;

const volatile struct {
	gid_t tgid; //PID to filter
	bool verbose; // Include non audit logs
	enum uniqueness unique_type; // Only unique info traces for same pid or cgroup

} tool_config = {};


struct event _event = {}; //Dummy instance for skeleton to generate definition

struct unique_key {
	int cap;
	u32 tgid;
	u64 cgroupid;
};

struct {
	__uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
	__uint(key_size, sizeof(u32));
	__uint(value_size, sizeof(u32));
} events
SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 10240);
	__type(key,
	struct unique_key);
	__type(value, u64);
} seen
SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 1);
	__type(key,	u64);
	__type(value, u64   );
} cgroup_id
SEC(".maps");

struct key_t {
  char comm[30];
};

int curr_cgroup_id = 0;

static __always_inline int record_cap(void *ctx, const struct cred *cred,
                                      struct user_namespace *targ_ns, int cap, int cap_opt) {
	u64 __pid_tgid = bpf_get_current_pid_tgid();
	gid_t tgid = __pid_tgid >> 32;
	pid_t pid = __pid_tgid;
	int audit;
	int insetid;

	if (LINUX_KERNEL_VERSION >= KERNEL_VERSION(5, 1, 0)) {
		// Field changed in v5.1.0
		audit = (cap_opt & 0b10) == 0;
		insetid = (cap_opt & 0b100) != 0;
	} else {
		audit = cap_opt;
		insetid = -1;
	}

	if (!tool_config.verbose && audit == 0) {
		return 0;
	}

	uid_t uid = bpf_get_current_uid_gid();

	struct event event = {
			.tgid = tgid,
			.pid = pid,
			.uid = uid,
			.cap = cap,
			.audit = audit,
			.insetid = insetid};

	if (tool_config.unique_type) {
		struct unique_key key = {.cap = cap};
		if (tool_config.unique_type == UNQ_CGROUP) {
			key.cgroupid = bpf_get_current_cgroup_id();
		} else {
			key.tgid = tgid;
		}

		if (bpf_map_lookup_elem(&seen, &key) != NULL) {
			return 0;
		}
		u64 zero = 0;
		bpf_map_update_elem(&seen, &key, &zero, 0);
	}

	bpf_get_current_comm(&event.comm, sizeof(event.comm));
	bpf_perf_event_output(ctx, &events, BPF_F_CURRENT_CPU, &event, sizeof(event));
	return 0;
}

SEC("kprobe/cap_capable")

int BPF_KPROBE(kprobe__cap_capable, const struct cred *cred,
               struct user_namespace *targ_ns, int cap, int cap_opt) {

    u64 pid = bpf_get_current_pid_tgid() >> 32;
    if (tool_config.tgid == pid) {
        u64 container_cgroup_id = bpf_get_current_cgroup_id();
        bpf_map_update_elem(&cgroup_id, &pid, &container_cgroup_id, BPF_NOEXIST);
    }

    u64 *tmp = bpf_map_lookup_elem(&cgroup_id, &tool_config);
    struct key_t key;

    if (tmp) {
        if ( bpf_get_current_cgroup_id() == *tmp) {
          bpf_get_current_comm(&key.comm, sizeof(key.comm));
          bpf_printk("before record_cap comm=%s pid=%d cgroupid=%d",key.comm, pid, *tmp);
          return record_cap(ctx, cred, targ_ns, cap, cap_opt);
        }
    }

	return 0;
}

char LICENSE[] SEC("license") = "GPL";
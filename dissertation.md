Micro-Hermes: A Userspace Simulation Toward an Open-Source eBPF Layer-7 Load Balancer

CS5099 Dissertation Draft

Student: 250014506

University of St Andrews Computer Science MSc [insert date of submission]

Abstract

Load balancers spread incoming network requests across a pool of servers so none are overwhelmed. This dissertation addresses that decision one level down: once traffic reaches a single machine, a Layer 7 (L7) load balancer, which processes application content rather than just packets, must decide which of its worker processes handles each new connection. Linux provides two mechanisms for this, epoll's exclusive wakeup and SO_REUSEPORT's stateless hash, however both blind to how busy each worker is. Because L7 work varies wildly in cost, this blindness concentrates load on a few workers, inflates tail latency, and keeps routing traffic to hung workers. Hermes (Pan et al., 2025), a production system at Alibaba Cloud, closes this gap. Workers publish their live status into shared memory, and a kernel eBPF program steers new connections towards available workers. Hermes is closed-source. Micro-Hermes is an open-source Rust implementation, built in two stages: a userspace simulation of the three-stage feedback loop, then a working version running a real kernel eBPF program over real sockets. Both are evaluated across the paper's four traffic regimes plus a fifth novel condition. The results support the paper's central claim: neither standard mechanism is safe in every regime, while Hermes routes around hung workers, balances connections an order of magnitude more evenly than epoll exclusive, and holds the best tail latency under overload. The eBPF version also reveals a cost the simulation missed: under very high rates of cheap connections, the system calls publishing worker status can outweigh the benefit.

Declaration

I hereby certify that this dissertation, which is approximately ??? words in length, has been composed by me, that it is the record of work carried out by me and that it has not been submitted in any previous application for a degree. This project was conducted by me at the University of St Andrews from ??? to ??? towards fulfilment of the requirements of the University of St Andrews for the degree of Computer Science MSc under the supervision of Dr Stephen McQuistin. 

In submitting this project report to the University of St Andrews, I give permission for it to be published online. I retain the copyright in this work.

[date] [signature]

Table of Contents

1. Introduction

1.1 Motivation

1.2 Problem Statement

1.3 Aims and Objectives

1.4 Report Structure

2. Background and Related Work

2.1 What Is a Load Balancer? Layer 4 vs Layer 7

2.2 A Brief History of Load Balancing

2.3 Layer 4 eBPF Load Balancing

2.4 Connection Scheduling Inside a Single Server

2.5 Prior Approaches to Intra-Server Scheduling

2.6 eBPF and Kernel Programmability

2.7 Layer 7 and Application-Aware Load Balancing

2.8 The Hermes Architecture

2.9 Positioning Micro-Hermes

3. Requirements Specification

4. Software Engineering Process

5. Ethics

6. Design

6.1 System Architecture Overview

6.2 Worker Status Table

6.3 Kernel Connection Dispatcher

6.4 Userspace Scheduler

6.5 The Two Baseline Mechanisms

7. Implementation

7.1 Shared Foundations

7.2 The Simulation

7.3 The eBPF Implementation

7.4 What Both Versions Simulate

7.5 Engineering Challenges

8. Evaluation and Critical Appraisal

8.1 Evaluation Framework and Methodology

8.2 Results: The Simulation

8.3 Results: The eBPF Implementation

8.4 Worker Hang Prevention Results

8.5 Scheduler Filter Behaviour

8.6 Comparing the Two Versions

8.7 Discussion

9. Conclusions

9.1 Summary of Contributions

9.2 Limitations

9.3 Remaining Work

Appendix A: Testing Summary

Appendix B: User Manual

Appendix C: Ethics Self-Assessment

References

1. Introduction

1.1 Motivation

Cloud providers place a Layer 7 (L7) load balancer in front of nearly every web service they host. These systems terminate TLS, parse and route HTTP, compress responses, and translate between protocol versions, at rates of hundreds of thousands of new connections per second. Internally, an L7 load balancer typically runs one worker process pinned to each CPU core, each with its own epoll event loop (Garrett, 2015). The operating system decides which worker receives each new connection, and the decision matters: a connection assigned to an overloaded worker queues behind expensive work and cannot escape, since established connections cannot migrate between workers.

Operational experience at Alibaba Cloud shows how badly this can go wrong. Pan et al. (2025) report a production incident in which a single worker, stuck in a read loop, dragged request latency from 30 milliseconds to 440 seconds for every connection pinned to it, while the kernel, unaware the worker was hung, continued to assign it new connections. The root cause is architectural: the two mechanisms Linux offers for distributing connections choose by the wakeup order of a kernel wait queue or by a stateless hash, and neither consults the one thing that predicts whether a worker can take on more work: its runtime status in userspace.

Hermes, the system Alibaba built in response, fixes this with a feedback loop: workers write their live status into shared memory, a lightweight userspace scheduler distils that status into a set of candidate workers, and an eBPF program inside the kernel forwards each new connection to one of the candidates. In production it reduced daily worker hangs by 99.8% and infrastructure unit cost by 18.9% (Pan et al., 2025). But Hermes is closed-source: its architecture is published, but its code, and therefore any independent validation of its claims, is not. This project implements the Hermes architecture in the open, in Rust, and tests whether its published behaviour can be reproduced outside Alibaba.

1.2 Problem Statement

This dissertation addresses two problems.

First, the technical problem inherited from Hermes: kernel connection dispatch is blind to userspace load. epoll's exclusive wakeup mode prefers the most recently registered worker, which concentrates connections onto a few workers. SO_REUSEPORT's hash spreads connections evenly on average but keeps blindly dispatching to workers that are overloaded, hung, or crashed. L7 processing cost varies by orders of magnitude between connections and is unknowable at dispatch time, so per-worker load diverges even under a perfectly uniform hash. Only userspace knows how loaded a worker really is, and only the kernel decides where connections go; a solution must carry information across that boundary cheaply and safely.

Second, the research problem this project takes on: Hermes demonstrates a solution at hyperscale, but as a closed system its results cannot be independently reproduced, its architecture cannot be studied or extended, and organisations without Alibaba's infrastructure cannot adopt it. Can the Hermes architecture be reimplemented faithfully from its published description, and does the reimplementation reproduce the qualitative behaviour the paper claims, such as its adaptability across traffic regimes and its time-bounded detection of hung workers?

1.3 Aims and Objectives

The aim of the project is a faithful, open-source implementation of the Hermes architecture, built in two stages. The first implements and evaluates the complete three-stage feedback loop as a userspace simulation, with the parts that would normally live inside the operating system replaced by stand-ins written in ordinary application code (objectives O1-O4). The second replaces those stand-ins with the real thing: a genuine in-kernel eBPF program, real sockets, and the kernel's own dispatch mechanisms (O5). Both versions are reported and evaluated here:

O1. Implement the full Hermes feedback loop in Rust: the shared-memory Worker Status Table with its three atomically updated metrics, the per-worker cascading-filter scheduler (Algorithm 1), and the per-connection dispatcher (Algorithm 2), preserving the paper's lock-free concurrency design and its division between kernel and userspace.

O2. Implement baseline models of the two main dispatch mechanisms, SO_REUSEPORT's stateless hash and epoll exclusive's LIFO wakeup, for comparison against Hermes under identical workloads.

O3. Reproduce the paper's evaluation methodology: its four traffic regimes (crossing high/low connection rate with low/high per-connection cost), an injected worker hang, and its primary balance metric (the standard deviation of worker connection counts), validating the implementation against the paper's qualitative rankings rather than only raw numbers.

O4. Extend the evaluation beyond the paper with a scenario that directly measures the cost of concentrating connections: a synchronised burst of requests from accumulated long-lived connections, the failure the Worker Status Table's connection-count metric exists to prevent.

O5. Port the dispatch path to a real eBPF program attached via SO_ATTACH_REUSEPORT_EBPF using the Aya toolchain, giving each worker a real listening socket and running the baselines as the kernel's own mechanisms rather than as models of them.

All five objectives were met. O1, O2 and O4 were achieved in full in the simulation. O3 was achieved with two deviations from the paper's rankings, which the simulation attributed to its own limitations rather than to the architecture; completing O5 made it possible to test that attribution directly, confirming one explanation and refuting the other (Section 8.6). O5 was the riskiest objective and was scoped as could-have from the outset (Chapter 3), because unlike O1-O4 it needs a running Linux kernel and code accepted by the kernel's own safety checker. Completing it turns the project from a study of the architecture into a working implementation of it, and makes the baseline comparison trustworthy: the mechanisms Hermes is measured against are no longer models written by the author but the kernel's own behaviour.

1.4 Report Structure

Chapter 2 provides necessary background information and reviews related work: what a load balancer is and why one is needed, the Layer 4 / Layer 7 distinction, the history of load balancing, the Linux mechanisms for distributing connections, and the research systems that have tried to improve on them, eBPF, and the Hermes architecture itself, closing with where Micro-Hermes sits in that body of work. Chapter 3 specifies the requirements the software had to meet, distinguishing those inherited from the Hermes paper from those the author added. Chapter 4 describes the development process, and Chapter 5 records ethical considerations. Chapter 6 presents the design, which both versions share. Chapter 7 covers the two implementations: first the parts common to both, then the simulation and the eBPF version separately, since this is where they differ. Chapter 8 evaluates them, establishing a common method, reporting each version's results, and then comparing the two directly, explaining where they agree, where they diverge, and why. Chapter 9 concludes with contributions, limitations, and remaining work. Appendices provide the testing summary, a user manual for building and reproducing every result, and the ethics self-assessment.

2. Background and Related Work

2.1 What Is a Load Balancer? Layer 4 vs Layer 7

A server is a program that handles requests sent to it by clients over a network; a single server can only handle so much traffic before it exhausts its CPU, memory, or network capacity. The standard fix is to run a pool of servers and place something in front of it that decides, for each incoming request, which member of the pool should handle it. That something is a load balancer. This dissertation addresses the same decision at a smaller scale: which worker process on a single server should take a given piece of work.

Network communication is conventionally described in stacked layers: lower layers move bytes between machines without caring what they mean, while higher layers interpret them. Two layers matter here. Layer 4 (L4), the transport layer, delivers a reliable, ordered stream of bytes between two endpoints (TCP is the relevant protocol); an L4 load balancer works at this level, selecting a backend by hashing the connection's addressing information, rewriting packet headers, and never inspecting the payload, so its per-connection work is small and predictable. Layer 7 (L7), the application layer, is where that byte stream is interpreted as an HTTP request, a TLS handshake, and so on; an L7 load balancer terminates the connection and operates on that content, performing TLS handshakes, parsing HTTP, routing on headers or paths, compressing responses, and translating between protocol versions (HAProxy Technologies, 2024). Widely deployed examples include HAProxy, NGINX and Envoy (HAProxy Technologies, 2024; Garrett, 2015; Envoy Proxy Authors, 2024).

Operating at L7 has two important consequences. First, per-connection cost is highly variable and unknowable in advance: one connection may need a full TLS handshake plus regex routing while its neighbour is a simple keep-alive request, and which is which only becomes apparent during processing. Classic L4 load metrics like queue depth are therefore poor proxies for L7 load. Second, the standard L7 architecture, popularised and documented in detail by NGINX, is a set of worker processes, one pinned to each CPU core, each running a run-to-completion event loop over epoll (Garrett, 2015). A connection's protocol state (TLS session, parser state, buffers) lives in the memory of the worker that accepted it, so established connections cannot migrate between workers. Every dispatch decision is permanent, which makes the dispatch mechanism extremely important.

A third consequence follows from multi-tenancy, the setting Hermes was built for. A cloud L7 load balancer serves many tenants from the same worker pool; Alibaba's architecture isolates them by rewriting each tenant's traffic to a distinct destination port, with a separate listening socket bound to each (Pan et al., 2025). Ports vastly outnumber workers (on the order of ten thousand against tens), so every worker serves many tenants at once, and when one worker is overloaded the latency penalty is shared by every tenant whose connections land on it. Inter-worker load balancing is therefore also what keeps tenants sharing a worker pool from degrading each other's performance. The port count matters mechanically too: with epoll's shared socket pattern, every worker registers interest in every port, so kernel-side wakeup work grows with the number of ports, a cost that resurfaces in the evaluation (Section 8.6).

2.2 A Brief History of Load Balancing

Load balancing, distributing incoming work across a pool of servers so that no one server becomes a bottleneck, is nearly as old as the web itself. The earliest widely deployed mechanism was round-robin DNS, in which a name server hands out a rotating list of addresses for the same hostname (Brisco, 1995). DNS balancing is coarse: it cannot see server load, cannot react to failures until clients' cached addresses expire, and offers no per-connection control. Through the late 1990s and 2000s the industry answer was the dedicated hardware appliance, a middlebox terminating traffic for a virtual IP and spreading it across backends, alongside early software alternatives such as the Linux Virtual Server project, which added Layer 4 load balancing directly into the Linux kernel (Zhang, 2000).

Hardware appliances scale poorly in terms of cost and flexibility, and hyperscalers eventually replaced them with software load balancers running on commodity machines. Microsoft's Ananta split the load balancer into a replicated control plane, which decided which backend pool each connection should go to, and a scale-out fleet of software "muxes" that did the actual forwarding, so capacity could be added simply by adding machines (Patel et al., 2013). Google's Maglev demonstrated that this approach could match the raw performance of dedicated hardware, saturating a 10 Gbps link from a single commodity server, and introduced consistent hashing for backend selection, ensuring that resizing the backend pool remaps only a small fraction of connections rather than most of them (Eisenbud et al., 2016). Subsequent research expanded on these ideas: Beamer removed per-connection state from the load balancer entirely, relying on state the backend servers already hold (Olteanu et al., 2018), and Cheetah made correct per-connection routing during pool changes a strict guarantee rather than a strong likelihood (Barbette et al., 2020).

All of these systems answer the question "which machine should serve this connection?". This dissertation is concerned with the question one level down: which worker process on that machine should serve it? The same imbalance problems occur (uneven work sizes, stateless assignment, workers that hang), but the mechanisms available are different, because the "scheduler" is now the operating system kernel and the "servers" are processes sharing its cores.

2.3 Layer 4 eBPF Load Balancing

A significant body of recent work applies eBPF at Layer 4 for high throughput load balancing across clusters of servers. Meta's Katran uses eBPF to perform hash based load balancing across backend servers at line rate, handling tens of millions of packets per second per core (Shirokov & Dasineni, 2018). Cilium similarly uses eBPF to provide load balancing and network policy enforcement across entire Kubernetes clusters (Cilium Authors, 2024).

Academic work continues in this direction. CRAB involves the load balancer only in the initial TCP handshake, after which the client talks to the chosen backend directly (Kogias et al., 2020); HEELS implements the same idea on ordinary commodity servers using eBPF (Yang & Kogias, 2023). RSS++ works beneath the transport layer: it measures how loaded each CPU core actually is and rewrites the lookup table the network card uses to spread packets across cores, so that busy cores receive fewer of them (Barbette et al., 2019). Of the systems discussed here it is the closest in spirit to Hermes, though it balances individual packets across cores rather than whole connections across worker processes.

All of these systems work at Layer 4 or below, on packets or on the handshake, before the application has touched the connection, so nothing about the application's state is visible to them, including how much work each worker process is carrying. They are built to spread traffic across infrastructure, not to keep the workers inside a single server evenly loaded.

2.4 Connection Scheduling Inside a Single Server

When multiple workers listen for the same incoming connections, the kernel must decide which worker accepts each one. Linux provides two mechanisms for this, and both were designed for correctness and throughput rather than balance.

These mechanisms sit on top of a lower-level kernel facility for tracking which connections have data ready. Early versions of this facility, select and poll, scan every watched connection on every call, which scales poorly once tens of thousands are held open. BSD's kqueue (Lemon, 2001) and Linux's epoll (Linux man-pages, 2023) replaced this with a stateful interface that reports only the connections that are actually ready, so its cost tracks how many are ready rather than how many are being watched. This is why epoll underpins almost every high performance Linux network server today.

The older dispatch pattern gives all workers one shared listening socket, which each registers with epoll. Historically, a new connection would wake every worker waiting on that socket (the "thundering herd"), and all but one of them would find nothing left to accept. The classic userspace workaround, still available in NGINX as accept_mutex, puts a lock around the accept step so that only one worker polls for new connections at a time (NGINX, 2024). Linux 4.5 addressed the problem directly in the kernel instead, adding the EPOLLEXCLUSIVE flag so that only one waiting worker is woken per event (Baron, 2015a; Corbet, 2015).

What matters for this dissertation is which worker gets woken. When a worker registers with epoll, it is inserted at the head of that socket's wait queue; on each wakeup the queue is traversed from the head, stopping at the first idle worker. Selection is therefore LIFO: the worker that registered most recently, which in steady state also tends to be the most recently active one, is preferred, so connections concentrate on a small number of workers while the rest sit idle. Cloudflare observed exactly this in production NGINX, with one worker absorbing the bulk of new connections while the last worker in the queue received almost none (Majkowski, 2017). A round robin wakeup mode, EPOLLROUNDROBIN, was proposed alongside EPOLLEXCLUSIVE specifically to fix this imbalance, but was never merged into the mainline kernel, in part because rotating the wait queue on every wakeup is unfriendly to CPU caches (Baron, 2015b; Corbet, 2015).

The newer pattern is SO_REUSEPORT, added in Linux 3.9, which lets each worker bind its own listening socket to the same port. The kernel picks which worker's socket receives each new connection by hashing the connection's 4-tuple (source and destination IP and port), a fixed, stateless calculation that does not depend on worker load (Kerrisk, 2013). Each worker now has its own private accept queue, so the contention and thundering herd problems disappear entirely; NGINX's adoption of this approach, which it calls socket sharding, measured 2-3x higher connection throughput (NGINX, 2015). But the hash has no awareness of worker state: it has no way of knowing that a particular worker is stuck in a slow TLS handshake, or hung, or has crashed, so it keeps sending that worker its fixed 1 in N share of new connections regardless.

Altogether the two mechanisms fail in opposite ways. Exclusive wakeup is aware of load only in the crude sense of preferring whichever worker is not currently busy, but its LIFO bias still concentrates load on a few workers. Reuseport spreads load with no bias at all, but with no awareness of worker state either. Neither can take into account what only the worker itself knows: how many events it has pending, how many connections it is holding, or whether it is making any progress at all.

The problem is not something specific to epoll that a newer interface simply fixes. io_uring, Linux's next-generation asynchronous I/O framework (Linux man-pages, 2024), wakes waiting workers in its default mode in a fixed FIFO order, a different bias from epoll exclusive's LIFO but just as blind to worker load, and just as capable of producing uneven load (Pan et al., 2025). The issue is structural: any policy that fixes the wakeup order at the point a worker registers ends up dispatching connections without ever consulting the worker's actual state. Making worker state part of the kernel's dispatch decision requires a way to run custom, user-supplied logic at the exact moment a socket is selected, which is precisely what eBPF provides.

2.5 Prior Approaches to Intra-Server Scheduling

The problem of distributing work across CPU cores has a long history in operating systems research. Classical CPU schedulers, such as Linux's Completely Fair Scheduler, divide processing time equitably among threads (Molnar, 2007), but they equalise CPU time rather than request cost, and have no way to account for requests that are far heavier than others, such as one requiring a TLS handshake or real-time compression. Packet scheduling disciplines like weighted fair queuing similarly prevent any one flow from monopolising bandwidth (Demers et al., 1989), but operate on traffic rather than on the workers inside a server, so neither line of work addresses worker level fairness.

Research on randomised load balancing offers two ideas that help explain Hermes's design. The "power of two choices" rule states that adding just a small amount of load information to random assignment, sampling two queues and picking the shorter one, reduces the worst case queue length exponentially compared to picking completely at random (Mitzenmacher, 2001). Hermes's coarse candidate filter is an engineered version of the same idea: instead of hashing across every worker blindly, it first narrows the field to a subset vetted for reduced load. Join-Idle-Queue is structurally the closest theoretical ancestor: idle processors register themselves in a shared idle queue that dispatchers consult when assigning work, so load information is gathered separately from, and ahead of, the assignment decision (Lu et al., 2011). Hermes's candidate bitmap works the same way: workers update it asynchronously, and the kernel consults it cheaply as each connection arrives. This lineage provides the conceptual framework for Hermes, but no specific mechanism for routing connections between the worker processes of a single server.

A separate body of research addresses the same problem Hermes does directly: tail latency caused by assigning work to the wrong core. Affinity-Accept modified Linux so that a connection is accepted and handled on the same core that received its packets, giving each core its own accept queue and allowing idle cores to steal work from busy ones (Pesterev et al., 2012). This was intra-server connection placement a decade before Hermes, though motivated by cache locality rather than worker load.

Later systems replaced the kernel's networking and scheduling stack entirely. ZygOS is a specialised dataplane operating system in which cores steal microsecond-scale requests from one another (Prekas et al., 2017); Shinjuku adds preemption at microsecond granularity so that short requests do not wait behind long ones (Kaffes et al., 2019); Shenango and Caladan reassign entire cores between applications on microsecond timescales (Ousterhout et al., 2019; Fried et al., 2020). These systems offer stronger tail latency guarantees than any dispatch time policy can, because they correct a bad placement after the fact by stealing, preempting, or reassigning work that has already arrived. The cost is that they replace the operating system's I/O path: applications must be rewritten against new interfaces, and operators must run non-standard stacks.

A milder version of the same idea is the userspace dispatcher, used by systems such as PostgreSQL, where a single process collects all I/O events and hands them out to backend workers under an explicitly fair policy (PostgreSQL Global Development Group, 2024). This works when the backend work is expensive relative to the cost of dispatching it; for a load balancer receiving hundreds of thousands of new connections per second, the dispatcher itself becomes the bottleneck. This is why Hermes leaves dispatch in the kernel and places its scheduler inside the workers that already exist, rather than adding a separate process to do the job (Pan et al., 2025).

The kernel community has recently moved in the same direction. sched_ext, merged in Linux 6.12, allows an eBPF program to define the kernel's process scheduling policy, with the verifier ensuring the program is safe to run and a watchdog reverting to the default scheduler if it misbehaves (Linux Kernel Documentation, 2024). The idea is the same as Hermes's, a scheduling policy written in userspace but executed safely inside the kernel through eBPF; only the object being scheduled differs. That mainline Linux has adopted this pattern for the harder, more general problem of scheduling processes suggests eBPF-driven dispatch is a lasting interface rather than a one off trick.

Hermes therefore takes a deliberately different approach from the dataplane systems. It keeps standard Linux, standard epoll, and the existing application structure, changes only the dispatch decision, and accepts that a connection cannot be moved once it has been placed (Section 2.1). The premise is that for L7 load balancer workloads, getting the initial placement right, using live worker status to inform it, captures most of the benefit at a small fraction of the deployment cost. Micro-Hermes adopts the same premise.

2.6 eBPF and Kernel Programmability

The Berkeley Packet Filter began as a small in-kernel virtual machine for running user-supplied packet filters safely (McCanne & Jacobson, 1993). Linux's extended BPF (eBPF) generalises this into a kernel-wide extension mechanism. Programs are written with a restricted instruction set, then checked by a static verifier built into the kernel, which proves the program is memory safe and guaranteed to terminate before it is allowed to run at all; once verified, the program is compiled and attached to one of many hook points throughout the kernel, from system call tracing to network packet processing (Vieira et al., 2020; eBPF.io, 2024). This verified safety is what separates eBPF from kernel modules: a buggy eBPF program is simply rejected at load time, rather than being able to crash the kernel. The trade off is reduced programmability: no unbounded loops, no heap allocation, a bounded program size, and communication with userspace only through "maps", typed key/value structures that both the kernel program and userspace code can read and write.

In networking, eBPF programs attach at several layers. At the lowest, XDP runs programs directly in the network driver, enabling packets to be processed as fast as the network link can deliver them (Høiland-Jørgensen et al., 2018). Central to this project is the SO_ATTACH_REUSEPORT_EBPF socket option (Linux 4.5), which attaches an eBPF program to a group of sockets sharing a port through SO_REUSEPORT. The program overrides the kernel's default hash based selection and instead chooses which socket receives each incoming connection, using the bpf_sk_select_reuseport helper. This is the exact hook Hermes uses: it turns socket selection from a fixed hash calculation into a programmable decision, one that can take into account state pushed down from userspace through an eBPF map.

The hook is mature and already carries production traffic at several major operators. Meta uses it to migrate established listening sockets from old server processes to new ones during software releases, allowing a multi billion user service to restart without dropping connections (Naseer et al., 2020). NGINX uses it to make sure QUIC packets that share a connection ID are routed to the same worker (NGINX, 2021). Cloudflare contributed a related hook, sk_lookup, which lets an eBPF program choose the receiving socket before the kernel's standard lookup even runs (Fayed et al., 2021). What sets Hermes apart from all of these is that it closes a feedback loop, continuously adapting its socket selection to worker runtime status rather than following a rule that is static or defined by the application itself, like a release phase or a connection ID.

This project is implemented in Rust, targeting the Aya library for the eBPF phase. Rust was chosen because its ownership based type system catches memory safety bugs (like use after free or data races) at compile time, without needing a garbage collector, which suits systems level code on both sides of the kernel boundary (Matsakis & Klock, 2014). Aya is a library written entirely in Rust that compiles eBPF programs and manages their maps directly, without depending on libbpf, the standard C based toolchain (Aya Contributors, 2024). Section 3 (Requirements) returns to why this project specifically needs the guarantees Rust provides.

2.7 Layer 7 and Application-Aware Load Balancing

Layer 7 load balancers work at the application layer, so they can route on information the application itself uses, such as the request type, its headers, or which session it belongs to. HAProxy and NGINX do this in userspace as reverse proxies: they accept every connection themselves and forward it on to a backend (HAProxy Technologies, 2024). This is flexible, but a proxy of this kind is itself a multi-worker epoll server, so it suffers exactly the intra-server dispatch problem described in Section 2.4: the kernel spreads connections across its workers without knowing anything about what those workers are doing.

Academic work on L7 load balancers has mostly looked at the layer above the individual machine. Yoda keeps TCP connection state in a replicated store so that any load balancer instance can take over any connection (Gandhi et al., 2016); later systems reduce CPU cost by moving work onto SmartNICs (AccelTCP; Moon et al., 2020) or programmable switches. All of this treats the load balancer machine as a black box, leaving dispatch among its worker processes to the kernel mechanisms of Section 2.4, even though, as the Hermes authors report from production experience, it is the userspace handling of the workload rather than the kernel's connection management that accounts for most of an L7 load balancer's CPU time (Pan et al., 2025). Dispatch inside the server is therefore the part of the stack that has received the least attention, and improving it adds to the work above rather than replacing any of it.

XLB is a recent related system that removes the sidecar proxy from communication between microservices by putting the L7 load balancing logic directly into the kernel's socket layer with eBPF: instead of service A reaching service B through a proxy, the kernel consults an internal load map to decide which instance of B should handle the message, reporting up to 1.5x higher throughput and 60% lower latency than Istio and Cilium sidecars (Wang et al., 2026). Like Hermes, XLB shows that dispatch decisions made in the kernel and informed by live status can beat a proxy-based design. Unlike Hermes and Micro-Hermes, it is concerned with traffic between separate microservice instances rather than with dispatch among the workers inside one server, so the two approaches complement each other rather than compete.

2.8 The Hermes Architecture

Hermes, Alibaba's response to the blindness described in Section 2.4, was deployed in production in front of multi-tenant cloud traffic and published at SIGCOMM 2025 (Pan et al., 2025). Its central idea is to make each worker's live runtime status a direct input to the kernel's dispatch decision, fed back via eBPF. The architecture takes the form of a closed feedback loop with three stages.

Stage 1 (status update): each worker maintains three metrics in a shared-memory Worker Status Table (WST) as a side effect of its normal event loop. The timestamp of its most recent loop entry acts as a liveness signal, since a hung worker stops re-entering its loop. The number of epoll events delivered but not yet handled acts as a proxy for instantaneous processing load (the paper found event count alone correlates well enough with processing time). The count of open connections guards against future overload when many idle, long-lived connections come online simultaneously. Each metric is an individual atomic integer (atomic meaning each read or write completes as one indivisible hardware step, so it can never be observed half-written), each worker writes only its own column of the table, and readers take no locks. A reader may occasionally see a value that is out of date, but never one that is corrupted, and stale reads are rare and essentially harmless to scheduling. Section 3 (Requirements) explains why avoiding a lock here matters enough to be a hard requirement.

Stage 2 (userspace scheduling): at the end of every event-loop iteration, each worker runs a three-stage cascading filter over the whole WST. It first drops workers whose timestamp is stale (hung), then drops workers with above-average connection counts, then drops workers with above-average pending events, with each average softened by an offset θ that prevents the candidate set from collapsing. The surviving candidate set is encoded as a bitmap and written into an eBPF map with a single atomic write operation. 

Stage 3 (kernel dispatch): on each new connection, an eBPF program attached via SO_ATTACH_REUSEPORT_EBPF reads the bitmap, hashes the connection's 4-tuple into the number of set bits, and selects the corresponding candidate's socket. If one or zero candidates survive, the program falls back to the kernel's default reuseport hash.

The division of labour between Stages 2 and 3 is deliberate: new connections can arrive at hundreds of thousands per second, far faster than any userspace scheduler can refresh its decision, so userspace performs coarse-grained filtering (a set of acceptable workers) and the kernel performs fine-grained per-connection selection within that set. Publishing a single "best" worker instead would funnel every connection arriving between updates onto it. Equally deliberate is keeping the scheduler in userspace. The kernel lacks application context, and eBPF's restricted programmability would make the filter logic and its dynamic policy updates awkward to express, so only the final decision (a bitmap packed into a single integer) crosses the boundary.

The paper's evaluation characterises traffic along two axes, connections per second and average per-connection processing time, giving four traffic regimes, and measures each mechanism in each regime at three levels of offered load (the rate of incoming work, independent of whether the system keeps up). Production measurements make the regimes concrete: across four global data-centre regions, the long-lived-connection regime (Case 3) accounts for 56.2% of traffic on average and the expensive-processing regime (Case 4) for another 31.7%, and these two dominant regimes are precisely where epoll exclusive and reuseport respectively perform worst (Pan et al., 2025). No single existing mechanism wins in all four regimes; Hermes's design goal is to be best or near-best in every one. Chapter 8's evaluation adopts the same framing, valuing adaptability across regimes as opposed to dominance in any one.

The measured cost of Hermes is small, 0.674%–2.436% of CPU depending on load, dominated under heavy load by the map update system calls rather than by the eBPF dispatcher itself (Pan et al., 2025). The production impact was large: a 99.8% reduction in daily worker hangs, an 18.9% reduction in infrastructure unit cost, and per-worker connection count balance (standard deviation 20) an order of magnitude better than epoll exclusive (3,200) and better than reuseport (50). Hermes, however, is closed-source. The paper publishes the architecture and algorithms in enough detail to reimplement, but no code or data. This gap was the primary motivation for this project.

2.9 Positioning Micro-Hermes

The related work above reveals a clear gap. Layer 4 eBPF load balancing is mature and well studied (Section 2.3). Intra-server scheduling research achieves strong guarantees, but only by replacing the operating system's stack (Section 2.5). Application aware dispatch that works with stock Linux exists only in proprietary industry systems like Hermes, with no open-source, reproducible implementation to validate or extend the approach.

Micro-Hermes fills that gap with an open-source, single-node implementation of the Hermes architecture. It uses the same SO_ATTACH_REUSEPORT_EBPF hook, implements the Worker Status Table in shared memory, and benchmarks the result against the standard Linux mechanisms. This validates the original paper's claims at a smaller scale and gives smaller organisations a foundation to build on.

3. Requirements Specification

The system described in Pan et al. (2025) is the specification: functional requirements FR1-FR7 and non-functional requirements NFR1-NFR2 are derived directly from the paper's architecture and implement objectives O1-O3 (Section 1.3). FR8 and NFR3-NFR5 are the author's own additions, supporting evaluation and reproducibility rather than the architecture itself, and are justified individually below; FR8 implements O4. FR9 implements O5, the eBPF port, scoped as could-have because it was judged materially riskier than O1-O4 within the available time; it was nonetheless completed, and the requirements below apply to both versions except where one is named specifically. Requirements are prioritised using MoSCoW (must/should/could).

Functional requirements:

FR1 (must). Implement the Worker Status Table in shared anonymous memory visible to all worker processes, holding the paper's three metrics per worker (loop-entry timestamp, pending-event count, open-connection count), partitioned so each worker writes only its own slot, using atomic i64's so reads need no locks.

FR2 (must). Implement Algorithm 1 (the scheduler): the cascading "time → connection count → event count" filter, with the offset θ set to half the candidate average (the paper's optimal ratio), producing a candidate bitmap written to the simulated eBPF map as a single atomic integer.

FR3 (must). Implement Algorithm 2 (the dispatcher): hash each new connection's 4-tuple, scale it (via the kernel's reciprocal_scale) into an index across the candidate workers in the bitmap, and pick the candidate at that index. If the bitmap has one or zero candidates, fall back to plain reuseport hashing.

FR4 (must). Implement the paper's event loop in each worker: record a timestamp at loop entry, collect events in batches with the paper's 5 ms timeout, track the busy count per event and the connection count on accept/close, and run a scheduling pass at the end of every iteration.

FR5 (must). Provide the two baseline dispatch policies on the same infrastructure for comparison: SO_REUSEPORT's stateless hash, and epoll exclusive's wait-queue wakeup. In the simulation both are models written by the author; in the eBPF version both are the kernel's own mechanisms, selected by how the listening sockets are set up (Section 6.5).

FR6 (must). Implement a workload generator that reproduces the paper's four traffic regimes (high/low connection rate crossed with low/high per connection cost), including long lived connections and a mid-run worker stall, paced at a configurable rate.

FR7 (must). Record two metric streams for evaluation: per connection timestamps (arrival, dequeue, completion) and per loop iteration values (a WST snapshot, plus for Hermes the scheduler's survivor counts at each filter stage and the resulting bitmap). Trials must be seeded and reproducible.

FR8 (should). Implement a fifth scenario beyond the paper's four, which measure how evenly connections are dispatched but not what an imbalance costs once it exists: a worker holding many quiet long-lived connections pays no visible penalty until they become active. FR8 fires a synchronised burst of follow-up requests across accumulated long-lived connections, turning the rationale behind the connection-count metric from an assumption into a measurement.

FR9 (could). Replace the simulated dispatch path with a real eBPF program attached via SO_ATTACH_REUSEPORT_EBPF, real SO_REUSEPORT sockets, and a real epoll loop, driven by a separate load generator over a real network connection.

Non-functional requirements:

NFR1 (must). No locks anywhere, including the WST, the candidate map, and the accept queues. A lock keeps concurrent readers and writers of shared memory safe by blocking every other worker until the holder releases it. That blocking is what this design cannot afford, as every worker's scheduler reads the entire WST at the end of every event-loop iteration, tens of thousands of times per second under heavy load, so a lock contended that often would become it's own bottleneck. Instead, following the paper, each field is individually atomic (readable and writable in one indivisible hardware step), so a read can never observe a torn or corrupted value, while the three fields read together may be momentarily out of sync with each other, exactly as the paper allows.

NFR2 (must). The kernel-facing code must be written within eBPF's constraints (no heap allocation, statically bounded loops), so that the port to a real eBPF program is a mechanical swap rather than a rewrite. Section 7.3 reports how far this held in practice.

NFR3 (must). The full benchmark matrix and every figure and table in the dissertation must regenerate from a single command with the same seed.

NFR4 (should). Where either version deviates from the real system (for example the simulated processing cost, which both versions share), the deviation must be documented and its effect on the conclusions discussed (Sections 7.5, 8.7).

NFR5 (should). The simulation should use only the Rust standard library plus libc (for mmap, fork, waitpid). The eBPF version necessarily relaxes this, since loading and attaching a kernel program requires the Aya toolchain (Section 7.3).

The project is implemented in Rust rather than C, the language the original paper and most eBPF tooling use, for two reasons. First, NFR1's lock-free design pushes all of its correctness onto the individual atomicity of shared fields; Rust's type system checks at compile time that shared memory is only ever touched through the atomic types this design requires, which C's compiler does not, and a violation would otherwise corrupt the WST silently rather than fail loudly. Second, Aya, the Rust eBPF toolchain used for O5 (Section 2.6), requires the whole codebase, kernel-facing and userspace code alike, to be Rust; choosing it from the outset avoided a language rewrite between the two versions.

4. Software Engineering Process

Three factors shaped the engineering process. As a replication, the specification was fixed externally by the Hermes paper, so the targets the evaluation had to hit were known before any code was written. The code is concurrent, and therefore easy to get subtly wrong. And as a single developer project on an 11 week timeline, both the process and the scope had to fit what one person could deliver.

Development began with a close reading of the paper, from which a design document was written recording every architectural commitment the implementation would need to honour. This document became the project's requirements baseline, from which Chapter 3 is derived. It also settled where corners could be cut: if the paper specified something, the implementation had to match it; if the paper left something open (such as the hang threshold constant), the choice made was recorded and justified.

Because the requirements were fixed up front, the process was plan driven at the level of architecture: the split between kernel-facing and userspace components was decided before any code was written, specifically so that the eBPF version could later replace components without restructuring the system (Section 6.1). Within that fixed architecture, implementation was iterative, proceeding in three increments, each started only once the previous was running end to end.

The first increment was a stripped-down preliminary simulator: four simulated cores running simple, non-atomic versions of the Hermes, LIFO, and reuseport dispatch algorithms, with no shared memory and no metrics collection. Getting this skeleton working before attempting the harder concurrency engineering surfaced the feedback loop's basic dynamics early and cheaply. The second increment built the skeleton out to the paper's full specification one component at a time, with unit tests written for each component before moving to the next; the test suite turns the paper's algorithms and fallback rules into executable checks. A fixed specification does not stop implementation problems from surfacing once real code exists: Section 7.5 records five of them, reported there rather than quietly fixed and left undocumented.

The third increment replaced the simulated kernel with the real one. This was deliberately left last because it is the only part of the project that cannot be developed on an ordinary development machine: it requires a running Linux kernel, administrator privileges, and code accepted by the kernel's safety checker. Porting only once the architecture was settled and the evaluation harness worked meant the port could be judged against results that already existed, rather than being debugged at the same time as the design. That ordering paid off: the components designed to carry over unchanged did carry over unchanged (Section 7.3).

Testing the system as a whole was benchmark driven. The rankings the paper expects in each traffic regime were written down as explicit validation targets before the benchmarks were run, and the analysis pipeline checks them automatically (Section 8.1). Because the targets were fixed in advance, one is now recorded as failing for the eBPF version and reported as such in Appendix A, rather than being retuned after the fact. Git was used throughout, with the analysis notebook, benchmark scripts and results all kept in the repository so that the whole evaluation can be reproduced from a clean checkout.

5. Ethics

The project raises no ethical concerns requiring approval. It involves no human participants, no user studies, no personal data, and no deployment against real traffic: all benchmark traffic is synthetic, generated and consumed within a single machine. The artefact evaluated is the author's own code. The system replicated is described in a peer-reviewed publication (Pan et al., 2025); no proprietary code, data, or confidential material from Alibaba was used or available — the reimplementation works exclusively from the published paper, which is standard and encouraged research practice (independent replication). The completed ethics self-assessment form is included as an appendix.

6. Design

6.1 System Architecture Overview

This chapter describes the design both versions share. One design covers both because the split was placed exactly where the real system's kernel/userspace boundary falls. Everything that lives in userspace in the real system, the Worker Status Table, the per-worker scheduler, and the dispatcher's selection logic, is written once and used unchanged by both versions. Only what belongs to the operating system differs: where connections come from, how a worker waits for them, and where the dispatch decision is executed.

The simulation replaces the operating system with a parent process. It generates synthetic connection arrivals at a configurable rate, computes a stand-in for the hash the kernel would derive from a connection's addresses and ports, runs the dispatch mechanism under test, and places each connection into the chosen worker's queue. Four forked child processes act as the workers; the Worker Status Table, the candidate bitmap, and the queues live in one region of shared memory mapped before forking.

The eBPF version removes the stand-ins. Connections arrive over a real network port from a separate load-generator process, each worker owns a real listening socket, and the dispatch decision is made by a small program running inside the kernel, consulted each time a new connection completes its handshake.

Because the dispatcher was written from the start under the restrictions the kernel imposes on programs it will accept (NFR2), it could be moved into the kernel rather than rewritten for it. The status table, the scheduler and its filters, the dispatcher's selection logic, and the definitions of all five traffic scenarios are identical in both versions.

In both versions the feedback loop is closed: a worker's load comes entirely from the connections the dispatcher assigned to it, and the dispatcher's decisions are driven by the bitmap the workers' schedulers publish, which is exactly the interaction the evaluation tests. Figure 1 shows both layouts side by side.

[FIGURE 1 HERE: side-by-side architecture diagram, one panel per version, shared parts aligned and shaded identically to show they are the same code.

LEFT PANEL, "simulation": parent process (generator → dispatcher → per-worker queues); below, 4 forked workers running the instrumented event loop with the scheduler (Algorithm 1) at loop end writing the bitmap to M_Sel; shared-memory strip carrying the WST, M_Sel, and queues.

RIGHT PANEL, "eBPF version": load generator → "real TCP connections" arrow → kernel box containing the eBPF program reading M_Sel and selecting a socket; below, 4 workers, each with its own listening socket and epoll instance, same event loop and scheduler, but writing M_Sel via a system call; shared-memory strip carrying the WST.

Caption: The two implementations side by side. The Worker Status Table, the scheduler and its cascading filters, and the dispatcher's selection logic are the same code in both. What differs is everything around them: in the simulation a parent process stands in for the kernel and connections are synthetic; in the eBPF version connections arrive over a real network port and the dispatch decision is executed inside the kernel.]

6.2 Worker Status Table

The WST holds, per worker, the three status metrics of Pan et al. (2025), each stored as an individually atomic 64-bit integer: the timestamp of the most recent event-loop entry (used for hang detection), the count of pending events delivered by epoll_wait but not yet handled (a proxy for instantaneous processing load), and the count of open connections (a guard against future overload from synchronised bursts on long-lived connections). The paper draws the table as one row per metric spanning all workers; this implementation transposes it into one slot per worker, which changes nothing about who reads or writes which field. Each slot is padded to a 64-byte cache line to prevent false sharing between workers on adjacent cores.

The concurrency design is preserved exactly: the table is partitioned by writer, each worker updates only its own slot, and the scheduler reads the whole table without locks. Reads may race with updates from other workers; as in the original system this is accepted by design, since per-field atomicity prevents torn values and a marginally stale metric does not meaningfully change a scheduling decision.

6.3 Kernel Connection Dispatcher

The dispatcher implements Hermes's Algorithm 2. For each new connection it reads the candidate bitmap from M_Sel, counts the set bits, and, if more than one candidate survives, scales the connection's hash into the candidate count using the Linux kernel's reciprocal_scale function (reimplemented exactly) and selects the Nth set bit of the bitmap. If one or zero candidates survive, it falls back to plain reuseport hashing across all workers, matching the paper's fallback rule: dispatching every connection that arrives between scheduler updates to a single "best" worker would itself overload it.

Only where this logic runs differs between versions. In the simulation it executes in the parent process, and M_Sel is a single shared atomic integer. In the eBPF version the same selection runs inside the kernel, and M_Sel is a kernel-held table of one entry that both sides can read and write. The same code serves both because of NFR2: it was written without heap allocation and with loops whose length is fixed at compile time, the conditions the kernel imposes on any program it will accept. Section 7.3 reports what this cost in practice.

6.4 Userspace Scheduler

Every worker runs Hermes's Algorithm 1 at the end of each event-loop iteration: there is no dedicated scheduler process, exactly as in the original design. As Section 2.5 established, a dedicated scheduler on the connection path is both a bottleneck and a single point of failure, whereas embedding the scheduler in every worker means scheduling continues as long as any worker is alive, a property the evaluation observes directly when a stalled worker is excluded by its peers (Section 8.4).

The scheduler snapshots the WST and applies three cascading filters in the paper's fixed priority order. The time filter removes workers whose last loop entry is older than a hang threshold (200 ms in this implementation; the paper leaves the constant implementation-defined). The connection-count filter and the pending-event filter each remove workers whose metric exceeds the candidate average plus an offset theta, which prevents the candidate set from collapsing when workers are near-uniformly loaded. Following the paper's finding that theta/avg = 0.5 is optimal, theta is half the candidate average, with a small floor so the filter remains permissive at cold start. The surviving set is encoded as a bitmap and published to M_Sel.

Because epoll_wait is bounded by a 5 ms timeout, every worker re-enters its loop, refreshes its timestamp, and re-runs the scheduler at least once every 5 ms even with no traffic, keeping hang detection and the candidate set live under idle conditions.

One consequence of this design is invisible in the simulation but significant in the eBPF version. Publishing the bitmap to a variable in shared memory is a single machine instruction; publishing it into a table the kernel owns requires a system call, which is orders of magnitude more expensive. Since the scheduler runs every loop iteration, the cost of publishing scales with how fast connections arrive, and under heavy traffic of cheap connections it becomes the dominant cost of the whole mechanism, which is what Section 8.3 measures and Section 8.6 explains.

6.5 The Two Baseline Mechanisms

Micro-Hermes is compared against the two standard Linux mechanisms described in Section 2.4. This is where the two versions differ most sharply: in the simulation both baselines are models written by the author, while in the eBPF version both are the operating system's own behaviour.

In the simulation, the reuseport baseline applies the same scaled-hash selection across all workers unconditionally, reproducing the stateless behaviour of the real mechanism: no awareness of worker state, and no scheduler at all. The epoll-exclusive baseline models the kernel's wakeup rule: waiting workers form a queue ordered by registration, with each new registration going to the front, and an arriving connection wakes the first idle worker found scanning from the head. The simulation reproduces this with state the generator can see: registration order is fork order, so the highest-numbered worker is the head; a worker counts as idle when its queue is empty and it has no pending events; and when no worker is idle the connection goes to whichever has the shortest backlog, approximating the shared queue the next free worker would drain. That last clause is the model's weak point, flagged as such in Section 8.6: choosing the shortest backlog is a load-aware decision the real mechanism cannot make, because under epoll exclusive every worker shares one socket and one queue, so there are no per-worker backlogs to compare.

The eBPF version removes the modelling entirely, without implementing either baseline, by changing only how the listening sockets are set up. For reuseport and Hermes, each worker opens its own listening socket on the shared port, and the kernel decides which socket receives each connection: by its own internal hash for the reuseport baseline, or by consulting the eBPF program for Hermes. For epoll exclusive, all four workers share one socket, each registering interest with the flag that asks the kernel to wake only one waiting worker per arrival; everything after that is the kernel's own wait-queue behaviour. The baseline numbers in Section 8.3 are therefore measurements of Linux, not of the author's understanding of Linux. Section 8.6 reports what happened to the simulation's conclusions once this was tested directly, and one of them does not survive.

In both versions, every worker runs the paper's 5 ms event-loop timeout under every policy, so no policy gets an artificial scheduling advantage from a different wakeup cadence (Section 7.5 explains why this is safe for the exclusive baseline).

7. Implementation

This chapter covers both implementations: what they share (Section 7.1), what is specific to each (Sections 7.2 and 7.3), one shared property that bounds what either can claim (Section 7.4), and the problems encountered along the way (Section 7.5).

7.1 Shared Foundations

Both versions are written in Rust, for the reasons given in Chapter 3, and both take the same components unchanged: the Worker Status Table, the scheduler and its three cascading filters, the dispatcher's selection logic, and the definitions of all five traffic scenarios. These are the substance of the architecture being replicated; what changes between versions is the machinery around them.

The simulation is about 1,750 lines across nine modules with a single dependency (libc, for mmap, fork and waitpid). The eBPF version adds roughly 2,100 lines across four crates: the program that runs inside the kernel, definitions shared between kernel and userspace code, the load-balancer process itself, and a separate load generator. Unit tests cover the scheduler's filters, the dispatcher's bit manipulation, and the shared-memory queue; the scheduler's tests carry over to the eBPF version unmodified, because the scheduler itself did.

In both versions the Worker Status Table lives in memory shared between processes, mapped before the workers are forked so that parent and children address the same physical pages. Each worker writes only its own slot, padded to a cache line so that workers updating their status do not contend for the same piece of memory.

7.2 The Simulation

All cross-process state lives in one structure mapped into shared memory before forking. Alongside the WST and M_Sel, the region contains one ring buffer per worker, standing in for the queue the kernel would keep for each listening socket. Each ring has exactly one producer (the generator) and one consumer (the owning worker), so the design remains lock-free throughout, mirroring the WST's partitioned-writer principle. A full ring rejects the connection, modelling the overflow that occurs when a real worker cannot accept quickly enough.

The parent process generates connections at the workload's target rate, pinning each arrival to an absolute point on the clock rather than sleeping a fixed gap between arrivals, so small timing errors do not accumulate over the run.

Each connection is created with three properties: a synthetic identifier standing in for the value the kernel would derive from a connection's addresses and ports (a counter scrambled with a fixed constant, varied between benchmark runs by a per-trial seed); a processing cost, sampled from a distribution; and a lifetime, after which the worker that received the connection closes it.

7.3 The eBPF Implementation

The eBPF version replaces every stand-in with the real thing. Three pieces of operating-system machinery do the work, and because they are the least familiar part of this dissertation they are described in plain terms.

The first is a way to run custom code inside the kernel safely. Ordinarily, code running inside an operating system kernel can crash the whole machine, which is why kernels do not accept code from applications. eBPF makes it possible anyway: a submitted program is checked by the kernel, which rejects anything it cannot prove will finish and will only touch memory it is entitled to touch. A program that passes is compiled and attached to a specific decision point. This is why the restrictions of NFR2 exist, and why they were respected from the first line of the dispatcher rather than retrofitted.

The second is a way for the kernel program and ordinary programs to share data. eBPF provides small typed tables, called maps, that both sides can read and write. Micro-Hermes uses two: one holding the number that encodes the candidate bitmap, written by the workers' schedulers and read by the kernel program on every connection; and one mapping worker number to listening socket, filled in once at startup and never changed.

The third is the decision point itself. When several sockets share a port, the kernel normally picks between them with a fixed internal calculation. The socket option this project uses replaces that fixed rule with a question put to our program: given this connection, which worker should take it? The program reads the bitmap, counts how many workers are currently acceptable, scales the connection's identifier into that count, and names the corresponding worker.

Two details of the port are worth recording. The fallback rule turned out to need no code at all: the paper specifies that when fewer than two candidates survive the filters, dispatch should fall back to the kernel's ordinary behaviour, and in this interface a program that simply declines to choose gets exactly that. What the simulation implemented as an explicit branch became, in the real system, the absence of one. Conversely, the injected worker stall could not carry over as written. In the simulation the load generator could stall a worker directly; in the eBPF version the generator is a separate process with no way to reach inside the load balancer, so the stall is instead requested of the load-balancer process itself at startup. The parameters are unchanged: worker 0, stalled for 400 ms, starting 1.5 seconds in.

The load generator is a separate program that opens real connections to the load balancer's port, sends each request carrying the processing cost it should incur, and times the round trip itself. Measuring from the client rather than inside the server is deliberate: it is what a real caller of the load balancer experiences, and it includes every cost along the path. Section 8.1 explains why this makes the two versions' latency figures non-interchangeable.

7.4 What Both Versions Simulate

One thing is not real in either version, and it bounds what either can claim. In both, a worker "processes" a connection by sleeping for that connection's assigned cost rather than performing actual Layer 7 work. Cost is therefore a property of the connection rather than of the worker, which matches how the paper describes real Layer 7 work: expense depends on what a connection involves (a TLS handshake, compression, a protocol translation) and varies from one connection to the next. In the eBPF version the cost is chosen by the load generator and sent with the request.

This keeps the traffic model identical across both versions and all fifteen benchmark points, which is what makes the comparison in Section 8.6 meaningful. The price is that neither version can reproduce the paper's CPU-overhead measurements, since the processor is idle during the sleep. The dispatch path in the eBPF version is real and Section 8.3 shows its cost appearing in the results; but the workload it is dispatching is not. This limitation is carried forward to Section 9.2.

7.5 Engineering Challenges

Building the preliminary simulator described in Chapter 4 paid off directly: the closed feedback loop and the generator's pacing were already validated, so the issues below could be isolated to the engineering added afterwards.

First, metrics collection across fork(): an early design accumulated benchmark records in a mutex-protected vector placed in the shared region, which is unsound; a vector's heap allocation is not in the shared mapping, and standard-library mutexes are undefined across processes on some platforms. Each worker instead buffers records privately and writes its own file shard, which the parent merges at the end, applying the WST's single-writer partitioning idea to the benchmarking plumbing itself.

Second, a modelling error in the simulation's epoll-exclusive baseline. An early version modelled the wait-queue head dynamically, as the worker with the most recent loop-entry timestamp, and gave idle baseline workers a periodic housekeeping timer. Because the workers are forked simultaneously, their timers fired in lock-step, rotating which worker was preferred about once a second and producing artificially balanced per-worker totals that masked the very pathology the baseline exists to demonstrate. The fix was to model what the kernel actually does: queue order is fixed at registration time, with the last-registered worker permanently at the head. With a static order, idle wakeups no longer reshuffle priority, so every policy can safely share the paper's 5 ms timer. The eBPF version retires this problem entirely, since it does not model the mechanism at all.

Third, batch sizing in the simulation. Real Layer 7 events cost microseconds, but the simulated per-event sleeps cost milliseconds, so a conventional batch limit would stretch a single loop iteration past the hang-detection threshold and starve the scheduler of fresh status data. The simulation's limit is therefore kept small, at 4 events, preserving the paper's property that scheduler frequency scales with load. The eBPF version retained the conventional limit of 64, the more realistic choice for a real event loop, but one that lets it spend far longer inside one iteration when events are expensive. Section 8.1 records how this difference affects the per-iteration measurements without affecting the per-connection ones.

Fourth, connection lifetime in the eBPF version. Cases 3 and 5 describe connections that stay open for 60 seconds, which in the simulation simply meant they never closed during a 4-second run. Taken literally by a real client holding a real socket, this would have blocked every benchmark point for a full minute after its traffic finished. The generator therefore caps how long it holds a connection at the run's duration plus a short grace period, preserving the intent that no connection closes while the run is in progress without stalling an automated matrix of 117 runs.

Fifth, the cost of publishing status. This appeared not as a defect but as a result, and it is the clearest example of something the simulation could not have shown. Writing the candidate bitmap is a single memory store in the simulation and a system call in the eBPF version, a cost paid on every loop iteration. Under Case 1 at heavy load it becomes the dominant cost of the entire mechanism and inverts the ranking (Sections 8.3 and 8.6). No adjustment was made in response; the result is reported as measured.

8. Evaluation and Critical Appraisal

This chapter evaluates both versions. Section 8.1 sets out the method, common to both. Sections 8.2 and 8.3 report each version's results, Sections 8.4 and 8.5 examine hang detection and the cascading filter, Section 8.6 compares the two versions directly, and Section 8.7 discusses what the evaluation does and does not establish.

8.1 Evaluation Framework and Methodology

The evaluation reproduces the four traffic regimes used by Pan et al. (2025), characterised by connections-per-second (CPS) and per-connection processing cost: Case 1, high CPS with low cost (a stress or traffic-spike scenario); Case 2, high CPS with high cost (a sustained overload at roughly 112% of aggregate worker capacity, with a 400 ms stall injected into one worker); Case 3, low CPS with low cost, but long-lived connections that never close within the run (the finance/chat pattern, and the most common case in Alibaba's production); and Case 4, low CPS with high cost (TLS- and regex-heavy web services). A fifth scenario, beyond the paper's four, repeats Case 3's accumulation of long-lived connections and then fires a follow-up request on every open connection simultaneously, providing direct evidence for the cost of connection concentration.

Following the paper's methodology, each case is additionally swept across three offered-load levels, light, medium and heavy, by scaling its connection arrival rate while holding its cost distribution fixed (Case 3's cost is trivial, so its levels instead scale how many long-lived connections accumulate). The per-case analysis in Sections 8.2 and 8.3 uses each case's characteristic level, the one at which its defining behaviour is clearest: light for Case 1, heavy for Case 2, medium for Cases 3 and 4. The full sweep is then reported for each version (Tables 2 and 4) to test whether the rankings hold as load varies, which is where the paper's own rankings are defined.

Each policy runs against every case and load level three times, with a per-trial seed varying the sequence of connection identifiers and sampled costs. Two record streams are collected: one row per completed connection, and one row per worker event-loop iteration (a WST snapshot plus, for Micro-Hermes, the scheduler's per-stage survivor counts). Three metrics match the paper's: latency (mean and 99th percentile), throughput (completed connections per second), and load balance (the standard deviation of per-worker connection counts). The full pipeline, from benchmark execution to every figure and table below, is reproducible from a single Jupyter notebook in the repository.

How the two benchmarks differ. Both versions ran exactly the same experiment: the same five cases, load levels, connection rates, cost distributions, injected stall and random seeds. This is what makes Section 8.6's comparison possible. But the two harnesses are not the same machinery, and three differences matter when reading their numbers side by side.

The first and most important is what "latency" measures. In the simulation everything happens inside one process: latency is the time from a connection being handed to a worker to the worker finishing the work, with no network, no socket and no connection setup involved. In the eBPF version latency is measured by the load generator, a separate process, from sending a request over a real connection to receiving the reply, and so includes connection establishment, the kernel's networking stack, and process scheduling. Client-side measurement was chosen because it is what a real user of a load balancer experiences.

The second is that a real connection can be refused or reset; these outcomes are recorded but excluded from latency statistics. The third concerns Case 5: the simulation's burst reaches whichever connections happen to be open at 2.5 seconds, roughly 150, while the eBPF version fires on all 240 it holds, so the two Case 5 medians describe differently sized events.

The consequence, applied throughout this chapter: the two versions may be compared on rankings and on trends as load rises, but not by putting one version's millisecond figure next to the other's.

One simplification shared by both versions must be stated before any results. Both listen on a single port; the system being replicated separates tenants by port and listens on roughly ten thousand (Section 2.1). This matters specifically for the epoll-exclusive baseline, whose dispatch cost grows with the number of ports while the other two policies' does not; Pan et al. give this as one of the two reasons epoll exclusive performs poorly in their Case 1. Both versions therefore test epoll exclusive in its most favourable configuration, and wherever it performs well below, that is a lower bound on its true cost. Section 8.6 returns to this.

A second, smaller difference: the simulation collects at most 4 events per loop iteration where the eBPF version collects up to 64 (the smaller limit stops the simulation's millisecond-scale processing stretching one iteration past the hang threshold, Section 7.5). The eBPF version can therefore spend much longer inside a single iteration, which is visible in its hang-detection trace (Section 8.4) and its sharper filter pruning (Section 8.5), but does not affect the per-connection latency or balance figures.

[TABLE 1 HERE — summary statistics for the simulation (regenerate from analysis/results; the committed summary_stats.tex now holds the eBPF version's figures, which appear as Table 3).

Caption: Simulation: latency and balance per dispatch policy across all five scenarios at each case's characteristic load level (mean over 3 trials; p99 shown ± sd across trials). Case 5 rows are burst follow-up requests, measured from the burst instant.]

8.2 Results: The Simulation

Throughout this section, the epoll-exclusive baseline is a model written by the author rather than the kernel's own behaviour, which matters for interpreting it (Sections 8.6 and 8.7). Table 1 summarises latency and balance for every scenario; Figure 2 shows the full latency distributions and Figure 3 the 99th percentile with its trial-to-trial spread.

[FIGURE 2 HERE — latency distributions, simulation (regenerate from analysis/results)

Caption: Simulation: latency distributions (queueing + processing) per dispatch policy, one panel per workload case at its characteristic load level, log x-axis, trials pooled. In the light-load cases (1 and 3) the three policies are indistinguishable; under load (Cases 2 and 4) reuseport's blind hash produces a heavy tail while Micro-Hermes routes around busy or stalled workers.]

[FIGURE 3 HERE — 99th-percentile bars, simulation (regenerate from analysis/results)

Caption: Simulation: 99th-percentile connection latency per policy (bars: mean of per-trial p99; error bars: min–max across trials; note the independent y-scales). Under load, reuseport's p99 exceeds Micro-Hermes's by roughly 30% in Case 2 and 2.2x in Case 4. The LIFO model's low latency in those cases is discussed in Section 8.6; Figure 6 shows the liability that accompanies it.]

Case 1 (high CPS, low cost). Far below capacity, all three policies deliver essentially the same latency: every policy completes a typical connection in 1.3 ms. They differ sharply in fairness. Across the 1,200 connections of a run, Micro-Hermes distributes most evenly (a standard deviation of 14 connections between workers, against 29 for reuseport), while the epoll-exclusive model sends every single connection to the wait-queue head worker (520): each request finishes before the next arrives, so the head worker is always idle again in time to be woken next. Micro-Hermes's tail is marginally higher than the baselines' (99th percentile 1.9 ms against 1.4 ms), for a structural reason: Stage 3 never picks the single best worker, only hashes each connection to one of Stage 2's candidates, so a few connections (about 0.6% in one trial) land on a candidate momentarily busier than an excluded worker. This matches the paper's ranking, which places reuseport marginally ahead of Hermes when load is trivially light: worker-awareness has nothing to protect against yet, so its overhead shows up as pure cost.

Case 2 (high CPS, high cost, with an injected hang). This case pushes offered load slightly past what the four workers can process, so queues grow throughout the run and latency is dominated by waiting. Awareness of worker status pays off directly: Micro-Hermes achieves a mean latency of 570 ms against reuseport's 784 ms, and a 99th percentile of 1,579 ms against 2,037 ms. The cause is simple: reuseport's hash does not know a worker is stalled, so it keeps sending roughly a quarter of new connections to the stalled worker for the duration of the stall, while Micro-Hermes routes around it. The epoll-exclusive model posts the lowest latency of all (mean 414 ms); whether this reflects the real mechanism is taken up in Section 8.6.

Case 3 (low CPS, long-lived connections). This case reproduces the paper's headline production result. Because connections never close within the run, every dispatch decision is permanent and mistakes accumulate. Figure 4 shows balance as a trajectory over the run, and Figure 5 shows who ends up holding the work. The epoll-exclusive model ends with a standard deviation of 104 connections, the head worker holding all 240 while the other three hold none, against 15 for Micro-Hermes and 14 for reuseport. This is the same ordering the paper reports from production (3,200, 50 and 20 for exclusive, reuseport and Hermes respectively). Micro-Hermes matches rather than beats reuseport here; Section 8.7 discusses why parity is the faithful expectation in this synthetic setting.

[FIGURE 4 HERE — balance over time, simulation (regenerate from analysis/results)

Caption: Simulation: standard deviation of per-worker open-connection counts during a Case 3 run (long-lived connections; mean over trials, bands span min–max). Micro-Hermes and reuseport hold the imbalance near-flat; under the LIFO model it grows linearly, since at this low arrival rate the head worker is always idle again before the next connection arrives.]

[FIGURE 5 HERE — concentration profile, simulation (regenerate from analysis/results)

Caption: Simulation: connections handled per worker in Case 3, ranked busiest to least busy within each trial (bars: mean over trials; error bars: min–max; dashed line: even share). The LIFO head worker takes all 240 connections in every trial; Micro-Hermes and reuseport sit close to the even share at every rank.]

Case 4 (low CPS, high cost). With expensive requests at moderate utilisation (about 75%), the danger is queueing a new connection behind a slow worker which is already busy. Micro-Hermes avoids this: its 99th-percentile latency of 442 ms is less than half of reuseport's 975 ms, which hashes blindly. The epoll-exclusive model again leads (295 ms), with Micro-Hermes tracking it as the paper predicts: Hermes's status detection reacts with a small delay where exclusive reacts immediately.

Case 5 (synchronised burst on long-lived connections). Cases 1 to 4 measure the latency of new connections, which cannot show why concentration is dangerous: a worker hoarding idle connections pays no visible penalty while they stay idle. Case 5 repeats Case 3's accumulation (roughly 150 open connections by 2.5 seconds) and then fires one 5 ms follow-up request on every open connection at the same instant, modelling the synchronised bursts (a market opening, a mass push notification) that the WST's connection count exists to guard against. Because a request on an established connection can only be processed by the worker that owns it, the burst is processed exactly as unevenly as the connections were dispatched. Under the epoll-exclusive model the head worker owns every connection and works through the entire backlog alone: the median follow-up waits 405 ms and the 99th percentile 801 ms. Micro-Hermes and reuseport, having spread the same connections across all four workers, complete the burst with a median of 107 ms and a 99th percentile of roughly 250 ms (Figure 6). The epoll-exclusive model's fast dispatch in Cases 2 and 4 coexists with a standing liability that comes due the moment its hoarded connections wake up.

[FIGURE 6 HERE — burst latency, simulation (regenerate from analysis/results)

Caption: Simulation: latency of follow-up requests when every open connection fires one simultaneously (Case 5, ~150 open connections at burst time, trials pooled). Under the LIFO model one worker owns every connection and serialises the entire burst (the straight-diagonal CDF is the signature of a single queue draining at a constant rate); Micro-Hermes and reuseport complete it 3 to 4x faster.]

Across all five cases Micro-Hermes never collapses: where it is not the best policy it trails by a fraction of a millisecond (Case 1) or a small margin (Cases 2 and 4), whereas each baseline fails badly in at least one regime: epoll exclusive on balance in Cases 1 and 3 and on burst latency in Case 5, reuseport on latency in Cases 2 and 4. This adaptability, rather than outright victory in any single regime, is the core property claimed for the Hermes architecture, and the simulation reproduces it.

Load sensitivity. Table 2 repeats the comparison at light, medium and heavy load for each of the four cases.

[TABLE 2 HERE — load sweep, simulation (regenerate from analysis/results; the committed load_sweep.tex now holds the eBPF version's sweep, which appears as Table 4)

Caption: Simulation: load sweep. Mean latency, P99 and throughput per policy at light/medium/heavy offered load in each case (mean over 3 trials). Every Case 2 level carries the injected stall.]

The sweep confirms the rankings are not artefacts of one operating point, and reproduces the paper's central claim that the value of worker-awareness grows with load. In Case 1, where the policies are indistinguishable at light and medium load, heavy load (75% utilisation) separates them: reuseport's mean rises to 13.4 ms and its 99th percentile to 33 ms as hash collisions land connections behind busy workers, while Micro-Hermes holds 3.7 ms and 24 ms, reproducing the paper's ranking flip for this case. In Cases 2 and 4 the gap over reuseport widens monotonically with load: Case 4's mean-latency ratio grows from 1.2x at light load to 2.8x at heavy. Throughput agrees: once offered load approaches capacity, reuseport can no longer sustain the arrival rate (at Case 2's medium level it completes 59 connections/s of the 75 offered where Micro-Hermes completes 69), because connections hashed onto overloaded workers sit in queues rather than completing. Case 3 shows latency parity at every level, as expected: its failure mode is balance, not latency. The epoll-exclusive model posts the lowest raw latency at every level of Cases 2 and 4, which the simulation attributed to its own modelling shortcut and treated as an optimistic bound. Section 8.6 tests that attribution against the real mechanism, and the answer is not the one the simulation expected.

8.3 Results: The eBPF Implementation

This section reports the same experiment run against the working system: real sockets, a real in-kernel dispatch program, and baselines that are the operating system's own behaviour rather than models of it. Table 3 gives the summary at each case's characteristic level and Table 4 the full load sweep; Figures 7 to 11 show the distributions, balance and burst behaviour underlying them.

[TABLE 3 HERE — analysis/tables/summary_stats.tex

Caption: eBPF version: latency and balance per dispatch policy across all five scenarios at each case's characteristic load level (mean over 3 trials; p99 shown ± sd across trials). Latency is measured by the client and includes connection setup and network-stack costs, so these figures are not directly comparable with Table 1's (Section 8.1).]

[TABLE 4 HERE — analysis/tables/load_sweep.tex

Caption: eBPF version: mean latency, p99 and throughput per policy at light/medium/heavy offered load in each case (mean over 3 trials). Every Case 2 level carries the injected stall.]

[FIGURE 7 HERE — analysis/figures/fig1_latency_cdf.pdf

Caption: eBPF version: latency distributions per dispatch policy, one panel per workload case at its characteristic load level, log x-axis, trials pooled. Latency is measured end to end by the client.]

[FIGURE 8 HERE — analysis/figures/fig2_p99_bars.pdf

Caption: eBPF version: 99th-percentile latency per policy (bars: mean of per-trial p99; error bars: min–max across trials; independent y-scales). Micro-Hermes holds the best tail in the overloaded Case 2 despite not holding the best mean.]

[FIGURE 9 HERE — analysis/figures/fig3_balance_over_time.pdf

Caption: eBPF version: standard deviation of per-worker open-connection counts during a Case 3 run. Micro-Hermes and reuseport hold the imbalance near-flat as connections accumulate; under real epoll exclusive it grows steadily, since the kernel keeps waking whichever worker is already free.]

[FIGURE 10 HERE — analysis/figures/fig4_concentration_profile.pdf

Caption: eBPF version: connections held per worker in Case 3, ranked busiest to least busy within each trial (dashed line: even share). Real epoll exclusive concentrates onto a small number of workers; Micro-Hermes and reuseport sit close to the even share at every rank.]

[FIGURE 11 HERE — analysis/figures/fig7_burst.pdf

Caption: eBPF version: latency of follow-up requests when every open connection fires one simultaneously (Case 5, 240 connections, trials pooled, measured from the burst instant). Under epoll exclusive the burst is served by an average of 2.0 workers against 4.0 for the other two policies, and takes roughly 3.4x longer at the tail.]

Case 1 (high CPS, low cost). At light and medium load the three policies are again close, with Micro-Hermes marginally ahead on both mean and tail (1.2 ms and 1.9 ms at light, against 1.3/2.3 for reuseport). Balance behaves as the architecture predicts: Micro-Hermes spreads most evenly (standard deviation 13.7 against reuseport's 14.5), while epoll exclusive concentrates severely (460), the same pathology the simulation's model showed.

At heavy load, however, the ranking inverts, and this is the most striking result in the chapter. Micro-Hermes's mean latency rises to 26.5 ms while epoll exclusive holds 3.0 ms and reuseport 5.1 ms, with all three sustaining essentially the offered rate. This contradicts the paper, which reports Hermes as the best performer in exactly this cell (measured means of 5.02 ms for Hermes, 5.10 ms for reuseport and 7.09 ms for epoll exclusive). Two differences plausibly account for it, and they pull in the same direction.

The first is the mechanism itself. Publishing the candidate bitmap costs a system call, paid at the end of every loop iteration. At 3,000 cheap connections per second the event loop turns over constantly and each turn pays that cost, while the work per connection is only a millisecond: the mechanism pays full price for information the workload is too uniform to need. This is consistent with the Hermes authors' own accounting, in which the system calls updating the eBPF map are the largest single component of their overhead under heavy load (Pan et al., 2025). The second is magnitude: they measure that component at under 1% of CPU, whereas the penalty here is a ninefold latency increase. The gap is explained by what the workers are doing: in production they perform real Layer 7 work, whereas here they sleep (Section 7.4), and a cost that is negligible beside real work is dominant beside no work. The direction of this finding is therefore trustworthy, but its size is an upper bound on the real penalty. Both differences are compounded by the single-port configuration (Section 8.1), which removes the O(#ports) cost that is the paper's other main reason epoll exclusive performs badly in this case. The honest summary is that this project has demonstrated a genuine cost of the design that the simulation could not surface, but has not reproduced the conditions under which the paper found that cost worth paying.

Case 2 (high CPS, high cost, with an injected stall). Under sustained overload, worker-awareness pays. Micro-Hermes's mean latency is 534.8 ms against reuseport's 586.5 ms, and its 99th percentile is the best of the three at 1,441 ms, against 1,651 ms for epoll exclusive and 1,654 ms for reuseport. It also completes more work than reuseport (75.2 against 70.4 connections/s). Epoll exclusive posts the lowest mean (419.3 ms) but the worst tail, and the split between those two numbers is the whole point: the mean says the average connection did well, the 99th percentile says the unlucky ones did badly, and it is the unlucky ones that concentration produces. That Micro-Hermes wins the tail on a case containing a genuine stalled worker is the result most directly supporting the architecture's purpose. The advantage over reuseport also grows with load, as the paper claims it should: at medium load Micro-Hermes's mean is 195.9 ms against reuseport's 343.5 ms, while at light load the two are level, because at 45% utilisation there is nothing yet to route around.

Case 3 (low CPS, long-lived connections). Latency is uniform across policies at every level, as expected when per-connection cost is trivial. Balance is the real measurement, and it reproduces the paper's headline production result: epoll exclusive ends with a standard deviation of 103.5 connections against 5.4 for Micro-Hermes and 6.0 for reuseport, matching the ordering Alibaba report from production (3,200, 50 and 20). As in the simulation, Micro-Hermes matches rather than beats reuseport, for the reason given in Section 8.7.

Case 4 (low CPS, high cost). With expensive requests, Micro-Hermes again beats reuseport across the board and the margin widens with load: at medium load its mean is 153.9 ms against 212.2 ms; at heavy load, 242.3 against 307.4 ms, with a 99th-percentile ratio of 1.57x (820.6 against 1,290.7 ms). Throughput follows: at heavy load Micro-Hermes completes 43.7 connections/s against reuseport's 40.4, because connections sent to already-busy workers wait rather than finish. Epoll exclusive again posts the lowest mean and tail of the three.

Case 5 (synchronised burst). This is where epoll exclusive's low latency elsewhere is paid for. After the same accumulation of long-lived connections, every open connection fires one follow-up request simultaneously. Micro-Hermes completes the burst with a mean of 67.5 ms and a 99th percentile of 224.3 ms, with reuseport equivalent at 68.0 and 232.9 ms. Epoll exclusive takes 248.3 ms mean and 769.8 ms at the tail, roughly 3.4 times worse. The explanation is in the same data: averaged over trials, the burst is served by 4.0 workers under Micro-Hermes and reuseport but only 2.0 under epoll exclusive. Half the machine sits idle while the other half works through a backlog it alone accumulated, and no dispatch decision can fix this after the fact.

Taken together, the working system supports the same overall claim as the simulation, with one important addition. Micro-Hermes is never the worst policy when it matters: it holds the best tail latency under overload, balances connections an order of magnitude better than epoll exclusive, and avoids the burst liability entirely. But it is now clearly the worst choice in one specific regime, high volumes of cheap uniform work, for a reason intrinsic to the design rather than incidental to this implementation.

8.4 Worker Hang Prevention Results

Case 2 injects a 400 ms stall into one worker mid-run, reproducing the kind of hang that motivated the original Hermes paper; this is the mechanism the architecture exists for, so it is examined in both versions.

In the simulation, the recorded scheduler decisions show it working as intended: in every trial the stalled worker is removed from the candidate bitmap within the 200 ms hang threshold of the stall starting (56-192 ms across trials), and readmitted within milliseconds of re-entering its loop. The exclusion happens in two steps: usually the load filters remove the worker almost immediately, because it stalls while still holding a batch of unprocessed events; the time filter then guarantees exclusion by 200 ms regardless, once the worker's loop-entry timestamp has gone stale.

The eBPF version shows the same mechanism operating, and Figure 12 traces one episode end to end. The behaviour is messier than the simulation's, in an instructive way. The worker is excluded across the whole stall window, as designed, but is then excluded again, for far longer than the stall itself, while it works through the backlog that accumulated while it was frozen: its pending-event count jumps to nearly thirty immediately after recovery and takes well over a second to drain. Recovery from a hang is not instantaneous once the hang ends, because the worker is genuinely still overloaded; the scheduler treats it accordingly, which is correct behaviour that the simulation's cleaner picture understated.

[FIGURE 12 HERE — analysis/figures/fig5_hang_detection.pdf

Caption: eBPF version: hang detection during a Case 2 run with a 400 ms stall injected into worker 0 (shaded). Top: worker 0's pending-event count, which flatlines during the stall and then spikes as the backlog arrives. Bottom: worker 0's presence in the candidate bitmap: excluded across the stall, briefly readmitted when it resumes, then excluded again while it drains the backlog.]

In both versions the exclusion is carried out by the other workers' schedulers, since the stalled worker cannot run its own. This confirms the value of embedding the scheduler in every worker rather than running it as a separate process: the component that notices a failure is never the component that failed. Over the same window, the reuseport baseline keeps assigning its usual share of new connections to the stalled worker, exactly the failure mode that motivated Hermes's timestamp-based detection.

8.5 Scheduler Filter Behaviour

The per-stage survivor counts recorded at every scheduling pass show the cascading filter behaving as the paper describes, in both versions. Under light load (Cases 1 and 3) the filters barely prune: 3.7 to 3.8 of the four workers survive all three stages in the simulation, and all 4.0 in the eBPF version. This is the desired behaviour: when no worker is struggling, the scheduler should not be narrowing the field. The overloaded Case 2 prunes hardest: in the eBPF version the average candidate set falls from 2.8 workers after the liveness filter to 2.2 after all three stages (Figure 13), with Case 4 between the extremes; the simulation follows the same ordering (3.5 survivors after the liveness filter in Case 2, 2.6 after the third).

[FIGURE 13 HERE — analysis/figures/fig6_cascade_stages.pdf

Caption: eBPF version: mean number of workers surviving each stage of the scheduler's cascading filter, per case (Hermes runs; error bars: ±1 sd across trials). The light-load cases prune nothing; the overloaded Case 2 prunes hardest; only the two expensive cases see the liveness filter remove anyone.]

In both versions, Case 2 is where the liveness filter itself prunes hardest, and not only because of the injected stall: a worker that spends several hundred milliseconds inside a single loop iteration processing slow events looks, from the status table's point of view, exactly like a hung worker, and gets treated as one. This is arguably correct, since either way it cannot accept new work promptly. Overall, both versions match the paper's observation that the fraction of workers passing the coarse filter shrinks as load increases, while theta's margin stops the set collapsing to a single candidate, which would forfeit the kernel's fine-grained selection and trigger the fallback path. The sharper pruning in the eBPF version is consistent with its workers facing real costs the simulation did not impose.

8.6 Comparing the Two Versions

Building the same architecture twice, once against a simulated operating system and once against a real one, makes it possible to ask which of the simulation's conclusions were about the architecture and which were about the simulation. Because both versions ran the identical experiment, the differences between them are attributable to the machinery underneath.

What the simulation got right. The central claims survive the move to a real kernel unchanged. Epoll exclusive concentrates connections catastrophically, and the real mechanism does it as badly as the model predicted (a Case 3 standard deviation of 103.5 connections against the model's 104). Micro-Hermes beats reuseport whenever workers are busy or stalled, with the margin widening with load, in both versions. The burst liability in Case 5 is real in both, and hang detection works, carried out by the surviving workers' schedulers. For a system reconstructed from a paper with no access to its source, this is a substantial vindication of the modelling.

What the simulation could only partly explain. The simulated epoll-exclusive baseline was better informed than the mechanism it modelled. Under real epoll exclusive, all workers share one listening socket and one queue, so the kernel's only decision is which idle worker to wake; there are no per-worker backlogs to inspect. The simulated baseline reused the per-worker queues the other two policies genuinely require, and when every worker was busy it handed each connection to whichever queue was shortest, a load-aware decision the real mechanism cannot make. The model's low latency in Cases 2 and 4 was therefore read, at the time, as flattery from this shortcut, and the comparison as conservative towards Micro-Hermes. Removing the shortcut was one of the reasons for building the second version, and the outcome is informative but not decisive: the baseline is now the kernel's own wait-queue behaviour, yet epoll exclusive still posts the lowest mean latency in Cases 2 and 4. The shortcut was not what produced its advantage, and that piece of the simulation's reasoning does not survive.

It does not follow that the prediction itself was wrong, and this is where the project must stop short of a conclusion. The paper attributes epoll exclusive's poor showing to two costs, and this evaluation has only removed one of them from consideration. The other, the O(#ports) dispatch overhead described in Section 8.1, cannot appear in a single-port benchmark at all. The correct statement is that this project has refuted its own earlier explanation for the gap without establishing what the gap would be under production conditions, and that closing the question requires a multi-port benchmark rather than a better model.

What can be said with confidence is narrower and does not depend on port count. Waking whichever worker is idle is a very good heuristic for mean latency and needs no information to apply, but it buys that mean by loading a few workers heavily. That is precisely what produces the concentration in Case 3, the burst penalty in Case 5, and the worst tail latency of the three policies under Case 2's overload. Hermes's value lies in protection against the tail and against accumulated imbalance, which is what the production incidents motivating it involved. On mean latency against epoll exclusive, this project's evidence is inconclusive.

What only the real system could show. In the simulation, publishing the candidate bitmap is a single memory instruction; in the eBPF version it is a system call paid at the end of every loop iteration, and under Case 1 at heavy load it inverts the ranking completely: 26.5 ms mean for Micro-Hermes against 3.0 ms for epoll exclusive, in exactly the cell where the simulation reported Micro-Hermes as the best policy. This is the sharpest illustration of the general point that a simulation cannot price what it does not implement, and it is a real limit on where the architecture should be deployed rather than a defect in this implementation.

The overall picture is that the architecture's value is real but conditional. It is worth paying for when per-connection work is expensive or variable, when workers can stall, or when connections are long-lived enough for imbalance to accumulate. Under the conditions measured here, it is not worth paying for when work is cheap and uniform, because the cost of continuously publishing status then exceeds the value of the information; that boundary carries the two qualifications given in Section 8.3. The paper's own production data suggests the favourable condition is usually the one that holds: the two regimes that dominate Alibaba's traffic are long-lived connections and expensive processing (Section 2.8), exactly the two where Micro-Hermes performs well here.

8.7 Discussion

This section records what the evaluation does not establish. The first entry has already been described: the simulation's explanation for epoll exclusive's low latency was tested by the second version and failed (Section 8.6). It is recorded as a corrected explanation rather than quietly dropped, because the reasoning was sound on the evidence then available and its failure is itself a result. It is also the clearest argument this dissertation can make for carrying a replication through to a working system: the second version both revealed the error and made the remaining gap precisely stateable rather than merely suspected.

A second finding from the simulation does survive scrutiny. Micro-Hermes only matches, rather than beats, reuseport's balance in Case 3, whereas the paper's production data has Hermes ahead (standard deviation 20 against 50). The explanation given was that real connections vary widely in duration, which degrades a stateless hash over time, whereas Case 3's synthetic connections are identical and never close, so the hash stays near-optimal by construction and offers no weakness to exploit. The eBPF version reproduces the same parity (5.4 against 6.0), which is consistent with that explanation, since the workload remained synthetic in both. Testing this properly would require realistically varied connection lifetimes, which neither version has.

The remaining limitations are treated in full in Section 9.2 and only noted here. Processing is simulated in both versions (Section 7.4): a worker sleeps for its connection's assigned cost, so the paper's CPU-overhead profile, a ratio of overhead to useful work, cannot be checked against work that does not exist. The scale is deliberately small (four workers, one machine, three trials per point), so the results establish orderings and trends rather than production figures. And two of Hermes's production subsystems, proactive termination of connections pinned to a hung worker and cluster-level scaling, are out of scope.

Within these limits, the evaluation supports Hermes's central claims, at a cost boundary it can state precisely only because the mechanism was built twice.

9. Conclusions

9.1 Summary of Contributions

This dissertation set out to reimplement a closed-source production system from its published description alone, and to test whether that description is sufficient to reproduce the system's claimed behaviour. Its contributions are:

First, the implementation itself (objectives O1, O2 and O5): to the author's knowledge the first open-source implementation of the Hermes architecture, comprising the shared-memory Worker Status Table, the per-worker cascading-filter scheduler, and the hash-among-candidates dispatcher. It is delivered both as a userspace simulation and as a working system in which the dispatch decision is made by a real program inside the Linux kernel and the two baselines are the operating system's own mechanisms rather than models of them. The paper's lock-free concurrency design is preserved exactly in both.

Second, a two-stage methodology that lets the architecture be checked against itself (O1 and O3). The simulation and the working system implement the same design and run the identical experiment, so any difference in their results is attributable to what changed underneath. This separates conclusions about the architecture from conclusions about the modelling, and Section 8.6 uses it to identify one conclusion the simulation reached that the real system refutes.

Third, an independent reproduction of the paper's qualitative claims (O3). Across the four published traffic regimes, both versions reproduce the paper's central results: Hermes beats the stateless hash whenever workers are busy or stalled, with a margin that widens as load rises; it balances long-lived connections an order of magnitude more evenly than exclusive wakeup; it detects an injected worker stall; and it delivers the best tail latency of the three policies under sustained overload.

Fourth, an extension of the paper's evaluation (O4): the synchronised-burst scenario converts the paper's argument for the connection-count metric from a rationale into a measurement. In the working system the burst is served by 4.0 workers under Hermes and only 2.0 under epoll exclusive, showing that exclusive's low average latency elsewhere is bought by hoarding connections that eventually become active all at once.

Fifth, a cost the original paper reports only as a percentage: the mechanism's overhead made visible in end-to-end latency. Publishing worker status to the kernel costs a system call on every loop iteration, and the loop turns over as fast as connections arrive, so this cost can dominate under high volumes of cheap work. That corroborates the paper's own finding that these system calls are the largest component of its overhead under heavy load. The magnitude measured here is not comparable with production, for the reasons given in Section 8.3, so the contribution is a demonstration that the cost is real, not a production estimate of it.

9.2 Limitations

The central limitation is that processing is simulated in both versions: a worker sleeps for its connection's assigned cost rather than performing real Layer 7 work such as a TLS handshake. The dispatch machinery in the eBPF version is entirely real, but the paper's CPU-overhead percentages express a ratio of overhead to useful work, and this system has no useful work to measure against.

Three further limitations follow from the evaluation's scale and workload. Four workers on one machine with three trials per point establish orderings and trends, not production performance figures. The connections are synthetic and uniform within each case, which is why Micro-Hermes matches rather than beats reuseport on balance in Case 3: a stateless hash only degrades when connection lifetimes vary widely (Section 8.7). And both versions listen on a single port where the real system uses roughly ten thousand, which removes a cost that falls on the epoll-exclusive baseline alone and leaves the mean-latency comparison against it unsettled (Sections 8.1 and 8.6).

Finally, two of Hermes's production subsystems are out of scope entirely: the proactive termination of connections already pinned to a hung worker, and the cluster-level detection and scaling that handle node-wide overload. Both address failures that a single node's dispatch decisions cannot fix.

9.3 Remaining Work

The most valuable next step is to replace the simulated processing cost with real Layer 7 work, so that a worker performing a TLS handshake or compressing a response actually consumes a processor rather than sleeping. This would put useful work in the denominator of the paper's overhead comparison, and would sharpen the Case 1 finding, where the dominant system-call cost is currently weighed against an idle processor.

A second direction is a multi-port benchmark. The wakeup cost that grows with port count (Section 2.1) is one of the paper's stated reasons for preferring its approach over epoll exclusive, and precisely the cost that would shift Cases 2 and 4, where exclusive currently leads. Unlike the other extensions, this one resolves an open question: whether exclusive's advantage would survive a realistic port count (Section 8.6).

A third is a workload with realistically varied connection lifetimes, which would test whether the paper's production advantage over reuseport reproduces outside production.

Beyond these, implementing the two subsystems noted as out of scope above would complete the reliability story, and the working system now makes the policy itself cheap to experiment with, which the closed original cannot offer: alternative filter orderings, a dynamically tuned θ, or entirely different candidate-selection rules can all be evaluated against the same harness and scenarios.

The broader conclusion is encouraging for replication research. A carefully written systems paper with no released code contained enough architectural detail to rebuild both its behaviour and its costs, and where the reimplementation diverged from expectation, the divergences were explicable and in one instance corrected a conclusion drawn from the simulation alone. That correction is the strongest practical argument for carrying a replication through to a working system rather than stopping at a model of one. Status-aware dispatch of the Hermes kind is not tied to hyperscale infrastructure: it is a small, portable idea, three shared counters, two filters and a bitmap, that this project makes available to anyone, along with a measured account of when it is worth using and when it is not.

Appendix A: Testing Summary

Correctness was established at three levels: unit tests over the algorithmic components, mechanical validation of whole-system behaviour against the paper's predictions, and integrity checks in the analysis pipeline.

Traceability to requirements. Each level of testing was chosen to close off a specific requirement from Chapter 3, rather than testing being an undirected activity performed after the fact. FR1 (the WST) and FR2/FR3 (Algorithms 1 and 2) are the requirements with the most precise published specification, so they receive direct unit tests: a test exists for each clause of each algorithm's definition, described below. FR4 (the instrumented event loop), FR5 (the baselines) and FR6 (the workload generator) are harder to test in isolation, since their correctness is behavioural rather than a fixed input/output mapping; these are instead verified through the whole-system predicates, which encode the paper's per-regime expectations and would fail if the loop, a baseline, or the generator misbehaved. FR7 (metrics recording) is verified by the analysis pipeline's data-integrity checks. NFR1 (lock-free concurrency) is exercised directly by the SPSC ring test's wrap-around and overflow cases. This mapping is what "the testing verifies the requirements" means in this project: every must-have requirement in Chapter 3 is the deliberate target of a named test or predicate below, not an incidental side effect of testing the code that happened to get written.

Unit tests (17 tests, run with cargo test) cover the components whose correctness the evaluation depends on:

- Algorithm 1 (scheduler): the time filter marks a worker unavailable once its loop-entry timestamp exceeds the hang threshold and readmits it after a fresh stamp; cold-start behaviour is permissive (a worker with no history is not spuriously filtered); the connection-count and event-count filters prune workers above average-plus-theta and no others; and theta's floor prevents the candidate set collapsing when all workers are near-uniformly loaded.
- Algorithm 2 (dispatcher): reciprocal_scale maps arbitrary 32-bit hashes into [0, n) exactly as the kernel's implementation does; Nth-set-bit selection returns the correct worker for every bitmap and index combination tested; and the fallback rule engages when the bitmap has one or zero set bits, dispatching by plain hash across all workers.
- The epoll-exclusive baseline (simulation only): the wait-queue model prefers the highest-registered idle worker and falls back to the shortest backlog only when no worker is idle. The eBPF version needs no equivalent test, because it does not implement this baseline at all — it configures the sockets so the kernel performs it.
- The shared-memory SPSC ring: values round-trip in order, a full ring rejects rather than overwrites, and head/tail indices wrap correctly past the buffer boundary.

The scheduler's tests transfer to the eBPF version unmodified, because the scheduler itself does. This is a small but concrete confirmation of the design boundary described in Section 6.1: the component that carries the architecture's logic was never coupled to how connections arrive.

Whole-system validation is benchmark-driven. The paper's qualitative expectations per traffic regime were written down as explicit, machine-checkable predicates before the benchmarks were run (for example: "Case 3: LIFO's open-connection SD exceeds three times either alternative's" or "all cases: Hermes never posts the worst P99 outside measurement parity"). The analysis notebook evaluates all eight predicates against every run and prints a pass/fail verdict table. Data integrity is asserted at load time: every expected policy × case × load × trial combination must be present, and no negative latencies may exist.

All eight predicates hold for the simulation. For the eBPF version, seven hold and one fails: the predicate requiring Micro-Hermes's 99th-percentile latency in Case 4 to beat reuseport's by more than 1.5x is not met, since the measured ratio is 1.40x (494.2 ms against 691.2 ms). The predicate is reported as failing rather than being relaxed to fit. Two things are worth noting about it. The direction of the result is unchanged — Micro-Hermes still beats reuseport on both mean and tail latency in Case 4, at every load level, and the margin still widens as load rises, reaching 1.57x at heavy load. What failed is a threshold chosen in advance against the simulation's numbers, which is exactly what a pre-registered threshold is for: it fails visibly when the system changes underneath it, rather than being quietly re-tuned afterwards.

Debugging during development relied on the VERBOSE per-iteration trace (every worker loop iteration with its WST snapshot and scheduling decision) and on the per-run console summary, which reports per-worker dispatch/completion/drop counts that must reconcile with the generator's totals. The two artefacts recorded in Section 7.5 — the unsound shared-memory Mutex&lt;Vec&gt; and the phase-locked timer artefact in the LIFO baseline — were both found through these traces. In the eBPF version the load balancer additionally logs its own startup sequence, which is where a failure to load or attach the kernel program surfaces, and the per-run script fails loudly if the load balancer exits before it is listening rather than silently producing an empty result file.

Appendix B: User Manual

The two versions have different requirements and are built and run separately. The simulation runs on any Unix-like system; the eBPF version requires Linux, because it loads a program into the Linux kernel.

Part 1: The Simulation

Requirements. A stable Rust toolchain (rustup.rs); any Unix-like OS with libc (developed on macOS, runs on Linux; no OS-specific code paths). The analysis pipeline additionally needs Python 3 with pandas, numpy and matplotlib, and Jupyter.

Building and running a single simulation, from the phase1/ directory:

    cargo run --release

Configuration is via environment variables:

    POLICY         hermes | lifo | reuseport      (default: hermes)
    WORKLOAD_CASE  1 | 2 | 3 | 4 | 5 | default    (default: default)
    LOAD           light | medium | heavy         (default: the
                   case's characteristic level)
    SEED           integer; varies the connection stream between
                   trials, reproducibly                (default: 0)
    METRICS_PATH   per-iteration tick CSV   (default: metrics.csv)
    CONNS_PATH     per-connection CSV       (default: conns.csv)
    VERBOSE        1 to print every worker loop iteration

For example, the overloaded compression-heavy case under the Hermes policy:

    POLICY=hermes WORKLOAD_CASE=2 LOAD=heavy cargo run --release

Each run prints a summary (per-worker dispatch/completion counts, balance SDs, and latency percentiles) and writes the two CSVs. Unit tests run with `cargo test`.

The simulation's full benchmark matrix (3 policies × {4 cases × 3 load levels + Case 5} × 3 trials, roughly 8 minutes) is driven by `analysis/run_benchmarks.sh`, which writes per-run CSVs into `analysis/results/`. Existing result files are skipped, so an interrupted run can be resumed; delete the directory to force a full regeneration.

Part 2: The eBPF Version

Requirements. Linux (developed against current Ubuntu). Beyond the stable Rust toolchain, the eBPF program needs the nightly toolchain with the `rust-src` component and the `bpf-linker` tool, since compiling code for the kernel's own instruction set is not part of the standard toolchain:

    rustup toolchain install nightly --component rust-src
    cargo install cargo-binstall && cargo binstall bpf-linker

Loading a program into the kernel and attaching it to a socket both require administrator privileges, so the load balancer is started under `sudo`. The `reuseport` and `lifo` policies do not strictly need this, since they involve no eBPF, but the benchmark scripts start every policy the same way for consistency.

Building both binaries, from the repository root:

    cargo build --release -p hermes -p hermes-bench

Running one benchmark point end to end, which starts the load balancer, waits for it to be listening, runs the load generator against it, and shuts it down:

    benchmark/run_case.sh <policy> <case> <load> <trial>

for example:

    benchmark/run_case.sh hermes 2 heavy 1

The full matrix, matching the simulation's:

    benchmark/run_all.sh          # 3 trials
    TRIALS=5 benchmark/run_all.sh # more trials for tighter error bars

Because every point starts and stops a real process under `sudo`, this is considerably slower per point than the simulation. Running `sudo -v` first, or configuring a passwordless rule for the `hermes` binary, avoids repeated password prompts across the matrix. Results are written to `benchmark/results/`, one file of per-connection records from the load generator plus one file of per-iteration records from each worker.

Part 3: Regenerating Figures and Tables

    cd analysis && jupyter lab hermes_analysis.ipynb
    # then: Kernel → Restart & Run All

or headless:

    jupyter nbconvert --to notebook --execute --inplace \
        analysis/hermes_analysis.ipynb

The notebook reads whichever results directory it is pointed at, runs the benchmark matrix if results are missing, and regenerates all figures (`analysis/figures/`, PNG and PDF) and tables (`analysis/tables/`, CSV and LaTeX). Note that the two versions write to different results directories, `analysis/results/` for the simulation and `benchmark/results/` for the eBPF version, and that the notebook's output paths are shared, so regenerating one version's artefacts will overwrite the other's. Point the notebook's `RESULTS_DIR` at the version being analysed, and direct its output elsewhere if both sets are needed at once.

Appendix C: Ethics Self-Assessment

[todo: attach the completed School of Computer Science ethics self-assessment / preliminary ethics form here, as referenced in Chapter 5.]

References

- Aya Contributors. (2024). Aya: An eBPF library for the Rust programming language. https://github.com/aya-rs/aya

- Barbette, T., Katsikas, G. P., Maguire Jr., G. Q., & Kostić, D. (2019). RSS++: load and state-aware receive side scaling. Proceedings of the 15th International Conference on Emerging Networking Experiments and Technologies (CoNEXT '19), 318–333. https://doi.org/10.1145/3359989.3365412

- Barbette, T., Tang, C., Yao, H., Kostić, D., Maguire Jr., G. Q., Papadimitratos, P., & Chiesa, M. (2020). A High-Speed Load-Balancer Design with Guaranteed Per-Connection-Consistency. 17th USENIX Symposium on Networked Systems Design and Implementation (NSDI 20). https://www.usenix.org/conference/nsdi20/presentation/barbette

- Baron, J. (2015a). epoll: add EPOLLEXCLUSIVE flag [kernel patch posting, merged in Linux 4.5]. https://lwn.net/Articles/667087/

- Baron, J. (2015b). epoll: introduce round robin wakeup mode [kernel patch posting, unmerged]. https://lwn.net/Articles/634781/

- Brisco, T. (1995). DNS Support for Load Balancing. RFC 1794, Internet Engineering Task Force. https://www.rfc-editor.org/rfc/rfc1794

- Cilium Authors. (2024). Cilium: eBPF-based networking, security, and observability. https://cilium.io/

- Corbet, J. (2015). Epoll evolving. LWN.net. https://lwn.net/Articles/633422/

- Demers, A., Keshav, S., & Shenker, S. (1989). Analysis and simulation of a fair queuing algorithm. ACM SIGCOMM Computer Communication Review, 19(4), 1–12.

- eBPF.io. (2024). What is eBPF? https://ebpf.io/

- Envoy Proxy Authors. (2024). Envoy: An open source edge and service proxy. https://www.envoyproxy.io/

- Eisenbud, D. E., Yi, C., Contavalli, C., Smith, C., Kononov, R., Mann-Hielscher, E., Cilingiroglu, A., Cheyney, B., Shang, W., & Hosein, J. D. (2016). Maglev: A Fast and Reliable Software Network Load Balancer. 13th USENIX Symposium on Networked Systems Design and Implementation (NSDI 16). https://www.usenix.org/conference/nsdi16/technical-sessions/presentation/eisenbud

- Fayed, M., Bauer, L., Giotsas, V., Kerola, S., Majkowski, M., Odintsov, P., Sitnicki, J., Chung, T., Levin, D., Mislove, A., Wood, C. A., & Sullivan, N. (2021). The ties that un-bind: decoupling IP from web services and sockets for robust addressing agility at CDN-scale. Proceedings of the ACM SIGCOMM 2021 Conference, 433–446. https://doi.org/10.1145/3452296.3472922

- Fried, J., Ruan, Z., Ousterhout, A., & Belay, A. (2020). Caladan: Mitigating Interference at Microsecond Timescales. 14th USENIX Symposium on Operating Systems Design and Implementation (OSDI 20). https://www.usenix.org/conference/osdi20/presentation/fried

- Gandhi, R., Hu, Y. C., & Zhang, M. (2016). Yoda: a highly available layer-7 load balancer. Proceedings of the Eleventh European Conference on Computer Systems (EuroSys '16). https://doi.org/10.1145/2901318.2901352

- Garrett, O. (2015). Inside NGINX: How We Designed for Performance & Scale. NGINX Blog. https://blog.nginx.org/blog/inside-nginx-how-we-designed-for-performance-scale

- HAProxy Technologies. (2024). HAProxy: The reliable, high performance TCP/HTTP load balancer. https://www.haproxy.org/

- Høiland-Jørgensen, T., Brouer, J. D., Borkmann, D., Fastabend, J., Herbert, T., Ahern, D., & Miller, D. (2018). The eXpress Data Path: Fast Programmable Packet Processing in the Operating System Kernel. Proceedings of the 14th International Conference on emerging Networking EXperiments and Technologies (CoNEXT '18), 54–66. https://doi.org/10.1145/3281411.3281443

- Kaffes, K., Chong, T., Humphries, J. T., Belay, A., Mazières, D., & Kozyrakis, C. (2019). Shinjuku: Preemptive Scheduling for µsecond-scale Tail Latency. 16th USENIX Symposium on Networked Systems Design and Implementation (NSDI 19). https://www.usenix.org/conference/nsdi19/presentation/kaffes

- Kerrisk, M. (2013). The SO_REUSEPORT socket option. LWN.net. https://lwn.net/Articles/542629/

- Kogias, M., Iyer, R., & Bugnion, E. (2020). Bypassing the Load Balancer Without Regrets. Proceedings of the 11th ACM Symposium on Cloud Computing (SoCC '20). https://marioskogias.github.io/docs/crab.pdf

- Lemon, J. (2001). Kqueue: A Generic and Scalable Event Notification Facility. Proceedings of the FREENIX Track: 2001 USENIX Annual Technical Conference, 141–153.

- Linux Kernel Documentation. (2024). Extensible Scheduler Class (sched_ext). https://www.kernel.org/doc/html/next/scheduler/sched-ext.html

- Linux man-pages. (2023). epoll(7) — Linux Programmer's Manual. https://man7.org/linux/man-pages/man7/epoll.7.html

- Linux man-pages. (2024). io_uring(7) — Asynchronous I/O facility. https://man7.org/linux/man-pages/man7/io_uring.7.html

- Lu, Y., Xie, Q., Kliot, G., Geller, A., Larus, J. R., & Greenberg, A. (2011). Join-Idle-Queue: A novel load balancing algorithm for dynamically scalable web services. Performance Evaluation, 68(11), 1056–1071.

- Majkowski, M. (2017). Why does one NGINX worker take all the load? Cloudflare Blog. https://blog.cloudflare.com/the-sad-state-of-linux-socket-balancing/

- Matsakis, N. D., & Klock, F. S. (2014). The Rust Language. Proceedings of the 2014 ACM SIGAda Annual Conference on High Integrity Language Technology (HILT '14), 103–104. https://doi.org/10.1145/2663171.2663188

- McCanne, S., & Jacobson, V. (1993). The BSD Packet Filter: A New Architecture for User-level Packet Capture. Proceedings of the USENIX Winter 1993 Conference, 259–270. https://www.usenix.org/conference/usenix-winter-1993-conference/bsd-packet-filter-new-architecture-user-level-packet

- Mitzenmacher, M. (2001). The Power of Two Choices in Randomized Load Balancing. IEEE Transactions on Parallel and Distributed Systems, 12(10), 1094–1104.

- Molnar, I. (2007). Modular scheduler core and completely fair scheduler. Linux Kernel Mailing List. https://lwn.net/Articles/230501/

- Moon, Y., Lee, S., Jamshed, M. A., & Park, K. (2020). AccelTCP: Accelerating Network Applications with Stateful TCP Offloading. 17th USENIX Symposium on Networked Systems Design and Implementation (NSDI 20). https://www.usenix.org/conference/nsdi20/presentation/moon

- Naseer, U., Niccolini, L., Pant, U., Frindell, A., Dasineni, R., & Benson, T. A. (2020). Zero Downtime Release: Disruption-free Load Balancing of a Multi-Billion User Website. Proceedings of the ACM SIGCOMM 2020 Conference, 529–541. https://doi.org/10.1145/3387514.3405885

- NGINX. (2015). Socket Sharding in NGINX Release 1.9.1. NGINX Blog. https://www.f5.com/company/blog/nginx/socket-sharding-nginx-release-1-9-1

- NGINX. (2021). Our Roadmap for QUIC and HTTP/3 Support in NGINX. NGINX Blog. https://blog.nginx.org/blog/our-roadmap-quic-http-3-support-nginx

- NGINX. (2024). Core functionality: accept_mutex. NGINX documentation. https://nginx.org/en/docs/ngx_core_module.html#accept_mutex

- Olteanu, V., Agache, A., Voinescu, A., & Raiciu, C. (2018). Stateless Datacenter Load-balancing with Beamer. 15th USENIX Symposium on Networked Systems Design and Implementation (NSDI 18), 125–139. https://www.usenix.org/conference/nsdi18/presentation/olteanu

- Ousterhout, A., Fried, J., Behrens, J., Belay, A., & Balakrishnan, H. (2019). Shenango: Achieving High CPU Efficiency for Latency-sensitive Datacenter Workloads. 16th USENIX Symposium on Networked Systems Design and Implementation (NSDI 19). https://www.usenix.org/conference/nsdi19/presentation/ousterhout

- Pan, T., Song, E., Zuo, Y., Zhang, S., Song, Y., Zhao, J., Hou, W., Lu, J., Sun, X., Zhang, S., Yang, Y., Zhang, J., Huang, T., Lyu, B., Li, X., Wen, R., Zong, Z., & Zhu, S. (2025). Hermes: Enhancing Layer-7 Cloud Load Balancers with Userspace-Directed I/O Event Notification. Proceedings of the ACM SIGCOMM 2025 Conference, 363–380. https://doi.org/10.1145/3718958.3750469

- Patel, P., Bansal, D., Yuan, L., Murthy, A., Greenberg, A., Maltz, D. A., Kern, R., Kumar, H., Zikos, M., Wu, H., Kim, C., & Karri, N. (2013). Ananta: Cloud Scale Load Balancing. Proceedings of the ACM SIGCOMM 2013 Conference. https://doi.org/10.1145/2486001.2486026

- Pesterev, A., Strauss, J., Zeldovich, N., & Morris, R. T. (2012). Improving network connection locality on multicore systems. Proceedings of the 7th ACM European Conference on Computer Systems (EuroSys '12), 337–350. https://doi.org/10.1145/2168836.2168870

- PostgreSQL Global Development Group. (2024). Overview of PostgreSQL Internals. PostgreSQL documentation. https://www.postgresql.org/docs/current/overview.html

- Prekas, G., Kogias, M., & Bugnion, E. (2017). ZygOS: Achieving Low Tail Latency for Microsecond-scale Networked Tasks. Proceedings of the 26th Symposium on Operating Systems Principles (SOSP '17). https://doi.org/10.1145/3132747.3132780

- Shirokov, N., & Dasineni, R. (2018). Open-sourcing Katran, a scalable network load balancer. Meta Engineering Blog. https://engineering.fb.com/2018/05/22/open-source/open-sourcing-katran-a-scalable-network-load-balancer/

- Vieira, M. A. M., Castanho, M. S., Pacífico, R. D. G., Santos, E. R. S., Câmara Júnior, E. P. M., & Vieira, L. F. M. (2020). Fast Packet Processing with eBPF and XDP: Concepts, Code, Challenges, and Applications. ACM Computing Surveys, 53(1), 1–36. https://doi.org/10.1145/3371038

- Wang, Y., Shou, C., Qian, J., & Liu, G. (2026). XLB: A High Performance Layer-7 Load Balancer for Microservices using eBPF-based In-kernel Interposition. arXiv preprint arXiv:2602.09473. https://arxiv.org/abs/2602.09473

- Yang, R., & Kogias, M. (2023). HEELS: A Host-Enabled eBPF-Based Load Balancing Scheme. Proceedings of the 1st Workshop on eBPF and Kernel Extensions (eBPF '23). https://doi.org/10.1145/3609021.3609307

- Zhang, W. (2000). Linux Virtual Server for Scalable Network Services. Proceedings of the Ottawa Linux Symposium 2000. http://www.linuxvirtualserver.org/ols/lvs.pdf

Micro-Hermes: A Userspace Simulation Toward an Open-Source eBPF Layer-7 Load Balancer

CS5099 Dissertation Draft

Student: 250014506

University of St Andrews Computer Science MSc [insert date of submission]

Abstract

Load balancers spread incoming network requests across a pool of servers so none are overwhelmed. This dissertation addresses that decision one level down: once traffic reaches a single machine, a Layer 7 (L7) load balancer, which processes application content rather than just packets, must decide which of its worker processes handles each new connection. Linux provides two mechanisms for this, epoll's exclusive wakeup and SO_REUSEPORT's stateless hash, however both blind to how busy each worker is. Because L7 work varies wildly in cost, this blindness concentrates load on a few workers, inflates tail latency, and keeps routing traffic to hung workers. Hermes (Pan et al., 2025), a production system at Alibaba Cloud, closes this gap. Workers publish their live status into shared memory, and a kernel eBPF program steers new connections towards available workers. Hermes is closed-source. Micro-Hermes is an open-source Rust implementation, built in two stages: a userspace simulation of the three-stage feedback loop, then a working version running a real kernel eBPF program over real sockets. Both are evaluated across the paper's four traffic regimes plus a fifth novel condition. The results support the paper's central claim: neither standard mechanism is safe in every regime, while Hermes routes around hung workers, balances connections an order of magnitude more evenly than epoll exclusive, and holds the best tail latency under overload. The eBPF version also reveals a cost the simulation missed: under very high rates of cheap connections, the system calls publishing worker status can outweigh the benefit.

Declaration

I hereby certify that this dissertation, which is approximately ??? words in length, has been composed by me, that it is the record of work carried out by me and that it has not been submitted in any previous application for a degree. This project was conducted by me at the University of St Andrews from ??? to ??? towards fulfilment of the requirements of the University of St Andrews for the degree of Computer Science MSc under the supervision of Dr Stephen McQuistin. 

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

Chapter 2 gives the background needed to follow the rest of the dissertation and reviews related work: what a load balancer is and why one is needed, the Layer 4 / Layer 7 distinction, the history of load balancing, the Linux mechanisms for distributing connections inside a single server and the research systems that have tried to improve on them, eBPF, and the Hermes architecture itself, closing with where Micro-Hermes sits in that body of work. Chapter 3 specifies the requirements the software had to meet, distinguishing those inherited from the Hermes paper from those the author added. Chapter 4 describes the development process, and Chapter 5 records ethical considerations. Chapter 6 presents the design, which both versions share. Chapter 7 covers the two implementations: first the parts common to both, then the simulation and the eBPF version separately, since this is where they differ. Chapter 8 evaluates them, establishing a common method, reporting each version's results, and then comparing the two directly, explaining where they agree, where they diverge, and why. Chapter 9 concludes with contributions, limitations, and remaining work. Appendices provide the testing summary, a user manual for building and reproducing every result, and the ethics self-assessment.

2. Background and Related Work

2.1 What Is a Load Balancer? Layer 4 vs Layer 7

Before going further, it helps to fix some basic vocabulary. A server is a program that handles requests sent to it by clients over a network; on its own, a single server can only handle so much traffic before it exhausts its CPU, memory, or network capacity. The standard fix is to run a pool of servers instead of one, and place something in front of that pool that decides, for each incoming request, which member of the pool should handle it, so that the pool as a whole absorbs far more traffic than any single server could alone. That something is a load balancer; deciding which of several available servers, or, as this dissertation is ultimately concerned with, which worker process on the same server, should take a given piece of work is the problem this whole dissertation addresses, applied at a smaller scale than the rest of this section.

Network communication is conventionally described in stacked layers, each handling a narrower part of the problem than the layer above it: lower layers move bytes between two machines without caring what those bytes mean, while higher layers interpret what the bytes actually say. This dissertation only needs two of these layers, referred to throughout by their conventional numbers. Layer 4, the transport layer, is concerned only with delivering a reliable, ordered stream of bytes between two endpoints (TCP is the relevant Layer 4 protocol here); a Layer 4 load balancer works at this level and never inspects what the stream contains. Layer 7, the application layer, is where that stream of bytes is actually interpreted, as an HTTP request, a TLS handshake, and so on; a Layer 7 (L7) load balancer works at this level, and both reads and acts on that content.

Load balancers are conventionally organised by which of these two layers they operate at. A Layer 4 (L4) balancer forwards packets: it selects a backend using the connection's 5-tuple, rewrites or encapsulates headers, and never inspects the payload. The quantity of work it has to do for a given connection is small, and it scales directly with how many packets a given connection sends as each packet is cheap and requires roughly the same amount of processing. A Layer 7 (L7) balancer, by contrast, terminates the connection and operates on application content: it performs TLS handshakes, parses HTTP, routes on headers or paths, compresses responses, and translates between protocol versions (HAProxy Technologies, 2024). Widely deployed examples include HAProxy, NGINX and Envoy (HAProxy Technologies, 2024; Garrett, 2015; Envoy Proxy Authors, 2024).

There are two important consequences of operating at L7. First, per-connection cost is highly variable and unknowable in advance. One connection may need a full TLS handshake plus regex routing while its neighbour is a simple keep-alive request; which is which only becomes apparent during processing. Therefore, using a classic L4 load metric like queue depth is a poor proxy for L7 load. Second, the standard L7 architecture is a set of worker processes, one pinned to each CPU core, each running a run-to-completion event loop over epoll. NGINX popularised this design and documents it in detail (Garrett, 2015). Migrating established connections across workers is not practical since the connection's protocol state (TLS session, parser state, buffers) lives in that worker's memory, and therefore connections are pinned to the worker that accepts them across their entire lifetime. Every dispatch decision is therefore permanent, which makes the dispatch mechanism extremely important.

A third consequence follows from multi-tenancy and is the setting Hermes was built for. A cloud L7 load balancer serves many tenants from the same worker pool. Alibaba's architecture isolates them by port: before traffic reaches the L7 LB, the layer below rewrites each tenant's port 80/443 traffic to a distinct destination port, and the L7 LB binds a separate listening socket to each port (Pan et al., 2025). This allows for per-tenant traffic management (rate limiting, accounting), but it also means ports vastly outnumber workers on the order of ten thousand ports against tens of workers, so every worker necessarily serves traffic from a large number of tenants at once. This makes the dispatch decision even more crucial as when one worker is overloaded, the latency penalty is not confined to one misbehaving tenant but shared by every tenant whose connections share the same worker. Inter-worker load balancing therefore does more than affect performance: it is what keeps tenants sharing a worker pool from degrading each other's performance. This design also matters mechanically for the comparison in this dissertation: with epoll's shared socket pattern, every worker registers interest in every port, so kernel-side wakeup work grows with the number of ports (a cost that will resurface in the evaluation) (Section 8.7).

2.2 A Brief History of Load Balancing

Load balancing, distributing incoming work across a pool of servers so that no one server becomes a bottleneck, is nearly as old as the web itself. The earliest widely deployed mechanism was round-robin DNS, standardised in the mid-1990s, in which a name server hands out a rotating list of addresses for the same hostname (Brisco, 1995). DNS balancing is coarse: it cannot see server load, cannot react to failures faster than a client's cached DNS address (TTL) expires, and offers no per-connection control, only per DNS lookup. Through the late 1990s and 2000s the industry answer was the dedicated hardware appliance, a middlebox terminating traffic for a virtual IP and spreading it across backends, alongside early software alternatives in the operating system kernel, most notably the Linux Virtual Server project, which added transport-layer (Layer 4) load balancing directly into Linux (Zhang, 2000).

Hardware appliances scale poorly in terms of cost and flexibility. As a result, hyperscalers eventually replaced them with software load balancers running on commodity machines. An early example is Microsoft's Ananta, which separated the load balancer into two distinct components. A control plane made routing decisions (which backend pool a connection should be sent to) and was replicated across multiple instances for fault tolerance since it did not need to touch every packet directly. A data plane, composed of a scale-out fleet of software "muxes" running on commodity hardware, handled the actual forwarding of traffic according to the control plane's decisions, and could be scaled simply by adding more machines (Patel et al., 2013).

Google's Maglev also demonstrated that this software-based approach could match the raw performance of dedicated hardware: a single commodity server was shown to saturate a 10 Gbps network link using ordinary machines rather than specialised appliances. Maglev also introduced the use of consistent hashing to determine which backend server should handle a given connection. This mattered because a naive hashing scheme would remap the vast majority of connections to different backends whenever a single server was added to or removed from the pool, which is highly disruptive for connections that rely on backend-held state. Consistent hashing ensures only a small proportion of connection-to-backend mappings change when the backend pool changes size (Eisenbud et al., 2016).

Subsequent research expanded on these ideas. Beamer removed the need for the load balancer to maintain any per-connection state at all, instead relying on state that the backend servers held prior to correctly route packets belonging to established connections (Olteanu et al., 2018). Cheetah went further still: where consistent hashing made misrouting unlikely during a pool change without ruling it out entirely, Cheetah made correct per-connection routing a strict guarantee by ensuring connections are always routed to the correct backend, even while the backend pool is changing (Barbette et al., 2020).

All of these systems answer the question "which machine should serve this connection?". This dissertation is concerned with the question one level down, which arises after the machine has been chosen: which worker process on that machine should serve it? The same imbalance problem occurs here; uneven work sizes, stateless assignment, workers that hang. However, the mechanisms available are different as the "scheduler" is the operating system kernel and the "servers" are processes sharing its cores.

2.3 Layer 4 eBPF Load Balancing

A significant body of recent work applies eBPF at Layer 4 (the transport layer) for load balancing across clusters of servers. These systems are designed for high throughput packet forwarding in data centres. Meta's Katran is a widely deployed example. It uses eBPF and XDP to perform hash based load balancing across backend servers (the same consistent hashing scheme described for Google's Maglev in Section 2.2) at line rate, handling tens of millions of packets per second per core (Shirokov & Dasineni, 2018). Cilium similarly uses eBPF to replace kube-proxy in Kubernetes environments, removing the overhead of iptables and providing load balancing and network policy enforcement across an entire cluster (Cilium Authors, 2024).

Academic work continues in this direction. CRAB involves the load balancer only in the initial TCP handshake; once the connection is established the client talks to the chosen backend directly (Kogias et al., 2020). HEELS takes the same idea and implements it on ordinary commodity servers using eBPF (Yang & Kogias, 2023). RSS++ works lower still, beneath the transport layer. A network card normally spreads incoming packets across CPU cores by hashing each packet into a fixed lookup table. RSS++ measures how loaded each core actually is and rewrites that table accordingly, so that busy cores receive fewer packets (Barbette et al., 2019). Of the systems discussed here it is the closest in spirit to Hermes, though it balances individual packets across cores rather than whole connections across worker processes.

All of these systems are relevant to this project, but the problem they solve is slightly different. They work at Layer 4 or below, on packets or on the handshake, before the application has touched the connection at all. At that point nothing about the application's state is visible to them, including how much work each worker process is currently carrying. What they decide is which backend machine, or which CPU core's packet queue, a piece of traffic goes to; whether a given worker process is sitting idle or overwhelmed is not something they can see. They are built to spread traffic across infrastructure, not to keep the workers inside a single server evenly loaded.

2.4 Connection Scheduling Inside a Single Server

When multiple workers listen for the same incoming connections, the kernel must decide which worker accepts each one. Linux provides two mechanisms for this, and both were designed for correctness and throughput rather than balance.

These mechanisms sit on top of a lower-level kernel facility for tracking which connections have data ready. Early Unix versions of this facility, select and poll, scan every watched connection on every call, which scales poorly once a load balancer is holding tens of thousands of them open. BSD's kqueue (Lemon, 2001) and Linux's epoll (Linux man-pages, 2023) replaced this with a stateful interface that reports only the connections that are actually ready, so its cost tracks how many are ready rather than how many are being watched, which is why epoll underpins almost every high performance Linux network server today

The older dispatch pattern gives all workers one shared listening socket and has each one register it with epoll. Historically, a new connection would wake every worker waiting on that socket (the "thundering herd"), and all but one of them would wake up only to find nothing left to accept. The classic userspace workaround, still available in NGINX as accept_mutex, avoids this by putting a lock around the accept step so that only one worker is allowed to poll for new connections at a time (NGINX, 2024). This gets rid of the herd, but at the cost of introducing a lock into the accept path. Linux 4.5 addressed the same problem directly in the kernel instead, adding the EPOLLEXCLUSIVE flag so that only one waiting worker is woken per event, rather than all of them (Baron, 2015a; Corbet, 2015).

What matters for this dissertation is which worker gets woken. When a worker registers with epoll, it is inserted at the head of that socket's wait queue. On each wakeup, the queue is traversed starting from the head, and traversal stops as soon as an idle worker is found. In practice this means selection is LIFO: the worker that registered most recently, which in steady state also tends to be whichever worker was most recently active, is the one preferred for new connections. The result is that connections concentrate on a small number of workers while the rest sit idle. Cloudflare observed exactly this behaviour in production NGINX, with one worker absorbing the bulk of new connections while the last worker in the queue received almost none (Majkowski, 2017). A patch adding a round robin wakeup mode, EPOLLROUNDROBIN, was proposed alongside EPOLLEXCLUSIVE specifically to fix this imbalance, but it was never merged into the mainline kernel, in part because rotating the wait queue on every wakeup is unfriendly to CPU caches (Baron, 2015b; Corbet, 2015).

The newer pattern is SO_REUSEPORT, added in Linux 3.9, which lets each worker bind its own listening socket to the same port instead of all workers sharing one socket. When a new connection arrives, the kernel picks which worker's socket receives it by hashing the connection's 4-tuple (source and destination IP and port), a fixed, stateless calculation that does not depend on anything about worker load (Kerrisk, 2013). Because each worker now has its own private accept queue rather than sharing one, the contention problem and the thundering herd problem from the older pattern disappear entirely. NGINX's adoption of this approach, which it calls socket sharding, measured 2-3x higher connection throughput (NGINX, 2015). The hash spreads connections evenly on average, but it has no awareness of worker state: it has no way of knowing that a particular worker is stuck in a slow TLS handshake, or hung, or has crashed, so it keeps sending that worker its fixed 1 in N share of new connections regardless of whether it is capable of handling them. It is also possible for the even spread to break down under heavy hitter traffic, since a hash collision can put a disproportionate number of high volume connections on the same worker.

Altogether the two mechanisms fail in opposite ways. Exclusive wakeup is aware of load only in the crude sense of preferring whichever worker is not currently busy, but its LIFO bias means load still ends up concentrated on a few workers. Reuseport spreads load with no bias at all, but with no awareness of worker state either. Neither mechanism can take into account the thing that actually matters and that only the worker itself knows: how many events it currently has pending, how many connections it is holding, or whether it is making any progress at all.

The problem is not something specific to epoll that a newer interface simply fixes. io_uring, Linux's next-generation asynchronous I/O framework (Linux man-pages, 2024), wakes waiting workers in its default interrupt mode in a fixed FIFO order, first worker registered is first worker woken. That is a different bias from epoll exclusive's LIFO order, but it is just as blind to worker load, and so it is just as capable of producing uneven load across workers (Pan et al., 2025). The underlying issue is structural, not a quirk of any one interface, so any policy that fixes the wakeup order at the point a worker registers, no matter what that order is, ends up dispatching connections without ever consulting the worker's actual state. Making worker state part of the kernel's dispatch decision requires a way to run custom, user-supplied logic at the exact moment a socket is selected, which is precisely what eBPF provides.

2.5 Prior Approaches to Intra-Server Scheduling

The problem of distributing work across CPU cores has a long history in operating systems research. Classical CPU schedulers, such as the Completely Fair Scheduler (CFS) in Linux, aim to divide processing time equitably among threads based on runtime metrics (Molnar, 2007). These schedulers, which operate at the process (OS/kernel-scheduling) level, are well studied, but they equalise CPU time rather than request cost. As such, they have no mechanism to account for the nature of individual network connections, or for the imbalance that arises when some requests are significantly heavier than others. For example, a request requiring a TLS handshake or real-time compression is far more CPU intensive, and far more variable in cost, than a typical connection on the same server.

Network scheduling has similarly been studied in the context of packet queuing and traffic shaping, with disciplines like weighted fair queuing (WFQ) designed to prevent any one flow from monopolising bandwidth (Demers et al., 1989). While these approaches are foundational, they operate at the traffic level rather than the application level and do not address the worker level fairness problem that arises inside a single server process.

Research on randomised load balancing offers two ideas that help explain Hermes's design. The "power of two choices" rule states adding just a small amount of load information to random assignment, sampling two queues and picking the shorter one, reduces the worst case queue length exponentially compared to picking a queue completely at random (Mitzenmacher, 2001). Hermes's coarse candidate filter can be understood as an engineered version of the same idea: instead of hashing across every worker blindly, it first narrows the field down to a subset vetted for reduced load. Join-Idle-Queue takes this further and is structurally the closest theoretical ancestor of Hermes's design. Here idle processors register themselves in a shared idle queue that dispatchers consult when assigning work, so gathering load information happens separately from, and ahead of, the actual assignment decision (Lu et al., 2011). Hermes's candidate bitmap works the same way: workers update it asynchronously in the background, and the kernel consults it at each connection's arrival, cheaply and without adding delay to the dispatch decision itself.

While this scheduling lineage shares the core goal of distributing uneven workloads fairly, they operate in different environments; cluster dispatchers, packet queues, or abstract queueing models. It provides the conceptual framework for Hermes, but not a specific mechanism for routing connections between the worker processes of a single server.

A separate body of research addresses the same problem Hermes does directly: tail latency caused by assigning work to the wrong core. The earliest of it stayed within the standard Linux kernel. Affinity-Accept modified Linux so that a connection is accepted and handled on the same core that received its packets, giving each core its own accept queue and allowing idle cores to steal work from busy ones. Locality alone produced substantial throughput gains (Pesterev et al., 2012). This was intra-server connection placement a decade before Hermes, though the motivation was cache locality rather than worker load.

Later systems replaced the kernel's networking and scheduling stack entirely. ZygOS is a specialised dataplane operating system in which cores steal microsecond-scale requests from one another, approximating the behaviour of a single shared queue (Prekas et al., 2017). Shinjuku adds preemption at microsecond granularity, using virtualisation hardware to interrupt long requests so that short ones do not wait behind them (Kaffes et al., 2019). Shenango and Caladan go further again and reassign entire cores between applications on microsecond timescales, keeping latency sensitive tasks responsive when they share a machine with other work (Ousterhout et al., 2019; Fried et al., 2020).

These dataplane systems offer stronger tail latency guarantees than any dispatch time policy can because they are able to correct a bad placement after the fact by stealing, preempting, or reassigning work that has already arrived. The cost is that they replace the operating system's I/O path: applications must be rewritten against new interfaces, and operators must run non-standard stacks. A milder version of the same idea is the userspace dispatcher, used by systems such as PostgreSQL, where a single process collects all I/O events and hands them out to backend workers under an explicitly fair policy (PostgreSQL Global Development Group, 2024). This works when the backend work is expensive relative to the cost of dispatching it. For a load balancer receiving hundreds of thousands of new connections per second it does not, because every connection must pass through the dispatcher and the dispatcher becomes the bottleneck. This is why Hermes leaves dispatch in the kernel and places its scheduler inside the workers that already exist, rather than adding a separate process to do the job (Pan et al., 2025).

The kernel community has recently moved in the same direction. sched_ext, merged in Linux 6.12, allows an eBPF program to define the kernel's process scheduling policy, with the verifier ensuring the program is safe to run and a watchdog reverting to the default scheduler if it misbehaves (Linux Kernel Documentation, 2024). The idea is the same as Hermes's, a scheduling policy written in userspace but executed safely inside the kernel through eBPF. What differs is the object being scheduled, as sched_ext assigns processes to cores while Hermes assigns connections to processes. This suggests eBPF-driven dispatch is a lasting interface rather than a one off trick, since mainline Linux has now adopted the same pattern for the harder, more general problem of scheduling processes themselves.

Hermes therefore takes a deliberately different approach from the dataplane systems. It keeps standard Linux, standard epoll, and the existing application structure, changes only the dispatch decision, and accepts that a connection cannot be moved once it has been placed (Section 2.1). The premise is that for L7 load balancer workloads, getting the initial placement right, using live worker status to inform it, captures most of the benefit at a small fraction of the deployment cost. Micro-Hermes adopts the same premise.

2.6 eBPF and Kernel Programmability

The Berkeley Packet Filter began as a small in-kernel virtual machine for running user-made packet filters safely at capture time (McCanne & Jacobson, 1993). Linux's extended BPF (eBPF) generalises this into a kernel-wide extension mechanism. Programs are written with a restricted instruction set, then checked by a static verifier built into the kernel, which proves the program is memory safe and guaranteed to terminate before it is allowed to run at all. Once verified, the program is compiled just in time and attached to one of many hook points throughout the kernel, ranging from system call tracing to network packet processing (Vieira et al., 2020; eBPF.io, 2024). This verified safety is what separates eBPF from kernel modules: a buggy eBPF program is simply rejected when it is loaded, rather than being able to crash the kernel. The trade off is reduced programmability: no unbounded loops, no heap allocation, a bounded program size, and communication with userspace only through typed shared "maps", key/value structures that both the kernel program and userspace code can read and write.

In networking, eBPF programs attach at several different layers. At the lowest layer, XDP runs programs directly in the network driver, before the kernel's own networking stack ever sees the packet, which enables line rate packet processing (handling packets as fast as the network link can deliver them) on ordinary hardware (Høiland-Jørgensen et al., 2018). Higher up the stack, and the layer central to this project, the SO_ATTACH_REUSEPORT_EBPF socket option (Linux 4.5) attaches an eBPF program to a group of sockets sharing a port through SO_REUSEPORT. This lets the program override the kernel's default hash based selection and instead choose which socket receives each incoming connection, using the bpf_sk_select_reuseport helper. This is the exact hook Hermes uses: it turns socket selection within a reuseport group from a fixed hash calculation into a programmable decision, one that can take into account state pushed down from userspace through an eBPF map.

This hook is not an obscure or experimental feature, it already carries production traffic at several major operators, which is evidence of how mature it is. Meta uses it to migrate already established listening sockets from old server processes to new ones during software releases, allowing a multi billion user service to restart without dropping connections (Naseer et al., 2020). NGINX uses it to make sure QUIC packets that share a connection ID are routed to the same worker (NGINX, 2021). Cloudflare contributed a related hook, sk_lookup, which lets an eBPF program choose the receiving socket before the kernel's standard lookup process even runs, decoupling a service from the specific IP address and port it is normally addressed by (Fayed et al., 2021). What sets Hermes apart from all of these existing methods is that it closes a feedback loop, continuously adapting its socket selection based on the worker's runtime status rather than relying on a logic which is static or defined by the application itself like a release phase, a QUIC connection ID, an addressing rule.

This project is implemented in Rust, and will target the Aya library once it reaches the eBPF phase. Rust was chosen because its ownership based type system catches memory safety bugs (like use after free or data races) at compile time, without needing a garbage collector to manage memory at runtime. That combination makes it well suited to systems level code on both sides of the kernel boundary, the userspace workers and, later, the eBPF programs themselves (Matsakis & Klock, 2014). Aya is a library written entirely in Rust that compiles eBPF programs and manages their maps directly, without depending on libbpf, the standard C based toolchain most eBPF projects use (Aya Contributors, 2024). Section 3 (Requirements) returns to why this project specifically needs the guarantees Rust provides.

2.7 Layer 7 and Application-Aware Load Balancing

Layer 7 load balancers work at the application layer, so they can route on information the application itself uses, such as the request type, its headers, or which session it belongs to. HAProxy and NGINX do this in userspace, usually as reverse proxies, as they accept every connection themselves and forward it on to a backend (HAProxy Technologies, 2024). This is flexible and gives them a large feature set, but it adds the latency of an extra hop. More importantly for this project, these proxies suffer from the intra-server dispatch problem described in Section 2.4. A proxy of this kind is a multi-worker epoll server, and the kernel spreads connections across its workers without knowing anything about what those workers are doing.

Academic work on L7 load balancers has mostly looked at the layer above the individual machine. Yoda separates the load balancer's connection state from the machines running it, keeping TCP state in a replicated store so that any instance can take over any connection, which makes the load balancer both highly available and able to scale horizontally (Gandhi et al., 2016). Later systems reduce its CPU cost by moving work onto hardware, either SmartNICs (AccelTCP; Moon et al., 2020) or programmable switches. All of this treats the load balancer machine as a black box. How connections are divided among the worker processes inside each instance is left to the kernel mechanisms of Section 2.4, even though, as the Hermes authors report from production experience, it is the userspace handling of the workload rather than the kernel's connection management that accounts for most of an L7 load balancer's CPU time (Pan et al., 2025). Dispatch inside the server is therefore the part of the stack that has received the least attention, and improving it adds to the work above rather than replacing any of it.

XLB is a recent related system. It removes the sidecar proxy from communication between microservices by putting the L7 load balancing logic directly into the kernel's socket layer with eBPF (Wang et al., 2026). Instead of service A reaching service B through a sidecar proxy, XLB intercepts the message in the kernel and consults an internal load map to decide which instance of B should handle it, reporting up to 1.5x higher throughput and 60% lower latency than Istio and Cilium sidecars. Like Hermes, XLB shows that dispatch decisions made in the kernel and informed by live status can beat a proxy-based design. Unlike Hermes and Micro-Hermes, it is concerned with traffic between separate microservice instances rather than with dispatch among the workers inside one server, so the two approaches complement each other rather than compete.

2.8 The Hermes Architecture

Hermes, Alibaba's response to the blindness described in Section 2.4, was deployed in production in front of multi-tenant cloud traffic and published at SIGCOMM 2025 (Pan et al., 2025). Its central idea is to make each worker's live runtime status a direct input to the kernel's dispatch decision, fed back via eBPF. The architecture takes the form of a closed feedback loop with three stages.

Stage 1 (status update): each worker maintains three metrics in a shared-memory Worker Status Table (WST) as a side effect of its normal event loop. The timestamp of its most recent loop entry acts as a liveness signal, since a hung worker stops re-entering its loop. The number of epoll events delivered but not yet handled acts as a proxy for instantaneous processing load (the paper found event count alone correlates well enough with processing time, whereas packet sizes are only known after processing). The count of open connections guards against future overload when many idle, long-lived connections come online simultaneously. Each metric is an individual atomic integer (atomic meaning each read or write completes as one indivisible hardware step, so it can never be observed half-written), the table is partitioned so each worker writes only its own column, and readers don't acquire a mutex/lock before reading. This lock-free design occasionally allows a read to be stale, but each metric's individual atomicity guarantees the value read is never corrupted, only possibly out of date. This is acceptable, since stale reads are rare and essentially harmless to the scheduling process. Section 3 (Requirements) explains why avoiding a lock here matters enough to be a hard requirement.

Stage 2 (userspace scheduling): at the end of every event-loop iteration, each worker runs a three-stage cascading filter over the whole WST. It first drops workers whose timestamp is stale (hung), then drops workers with above-average connection counts, then drops workers with above-average pending events, with each average softened by an offset θ that prevents the candidate set from collapsing. The surviving candidate set is encoded as a bitmap and written into an eBPF map with a single atomic write operation. 

Stage 3 (kernel dispatch): on each new connection, an eBPF program attached via SO_ATTACH_REUSEPORT_EBPF reads the bitmap, hashes the connection's 4-tuple into the number of set bits, and selects the corresponding candidate's socket. If one or zero candidates survive, the program falls back to the kernel's default reuseport hash.

The division of labour between Stages 2 and 3 is deliberate: new connections can arrive at hundreds of thousands per second, far faster than any userspace scheduler can refresh its decision, so userspace performs coarse-grained filtering (a set of acceptable workers) and the kernel performs fine-grained per-connection selection within that set. Publishing a single "best" worker instead would funnel every connection arriving between updates onto it. Equally deliberate is keeping the scheduler in userspace. The kernel lacks application context, and eBPF's restricted programmability would make the filter logic and its dynamic policy updates awkward to express, so only the final decision (a bitmap packed into a single integer) crosses the boundary.

The paper's evaluation characterises traffic along two axes, connections per second and average per-connection processing time, giving four traffic regimes, and measures each mechanism in each regime at three levels of offered load (the rate of incoming work, independent of whether the system keeps up). Hermes' production measurements make the regimes concrete: across four global data-centre regions, the long-lived-connection regime (Case 3) accounts for 56.2% of traffic on average and the expensive-processing regime (Case 4) for another 31.7%. These two dominant regimes are precisely where epoll exclusive and reuseport respectively perform worst (Pan et al., 2025). No single existing mechanism wins in all four regimes; Hermes's design goal is to be best or near-best in every one. Chapter 8's evaluation adopts the same framing, valuing adaptability across regimes as opposed to dominance in any one.

The measured cost of Hermes is small: 0.674%–2.436% of CPU depending on load, dominated under heavy load by the map update system calls rather than by the eBPF dispatcher itself, which costs at most 0.043% (Pan et al., 2025). The production impact however was large: a 99.8% reduction in daily worker hangs, an 18.9% reduction in infrastructure unit cost, and per-worker connection count balance (standard deviation 20) an order of magnitude better than epoll exclusive (3,200) and better than reuseport (50). Hermes, however, is closed-source. The paper publishes the architecture and algorithms in enough detail to reimplement, but no code or data. This gap was the primary motivation for this project.

2.9 Positioning Micro-Hermes

The related work above reveals a clear gap. Layer 4 eBPF load balancing is mature and well studied (Section 2.3). Intra-server scheduling research achieves strong guarantees, but only by replacing the operating system's stack (Section 2.5). Application aware dispatch that works with stock Linux, preventing some worker processes from becoming overloaded while others sit idle, exists only in proprietary industry systems like Hermes. No open-source, reproducible implementation exists to validate or extend this approach. 

Micro-Hermes fills that gap with an open-source, single-node implementation of the Hermes architecture. It uses the same SO_ATTACH_REUSEPORT_EBPF hook, implements the Worker Status Table in shared memory, and benchmarks the result against models of the standard Linux mechanisms. This validates the original paper's claims at a smaller scale and gives smaller organisations a foundation to build on. 

3. Requirements Specification

The requirements follow from the project's nature as a replication. The system described in Pan et al. (2025) is the specification, so functional requirements FR1-FR7 and non-functional requirements NFR1-NFR2 are derived directly from the paper's architecture and implement objectives O1-O3 (Section 1.3): they exist because the paper's design demands them, not because the author chose them. FR8 and NFR3-NFR5 are the author's own additions, made to support evaluation and reproducibility rather than to replicate the architecture itself, and each is justified individually below; FR8 implements O4. FR9 implements O5, the eBPF port, and was scoped as could-have because completing it within the available time was judged materially riskier than O1-O4; it was nonetheless completed, and the requirements below are written to be read as applying to both versions except where a requirement names one specifically. Requirements are prioritised using MoSCoW (must/should/could).

Functional requirements:

FR1 (must). Implement the Worker Status Table in shared anonymous memory visible to all worker processes, holding the three metrics of the paper (loop-entry timestamp, pending-event count, open-connection count) per worker, partitioned so each worker writes only its own slot, using atomic i64's to allow for reading without locks.

FR2 (must). Implement Algorithm 1 (the scheduler): the cascading "time → connection count → event count" filter, with the offset θ set to half the candidate average (the paper's optimal ratio), producing a candidate bitmap written to the simulated eBPF map as a single atomic integer.

FR3 (must). Implement Algorithm 2 (the dispatcher): given the candidate bitmap from the scheduler, hash the connection's 4-tuple and scale it (via the kernel's reciprocal_scale) into an index across the number of candidate workers, then pick the candidate at that index. If the bitmap has one or zero candidates, skip this and fall back to plain reuseport hashing instead

FR4 (must). Implement the paper's event loop in each worker: record a timestamp at loop entry, collect events in batches with the paper's 5 ms timeout, track the busy count per event, track the connection count on accept/close, and run a scheduling pass at the end of every iteration.

FR5 (must). Provide the two baseline dispatch policies on the same infrastructure for comparison: SO_REUSEPORT's stateless hash, and epoll exclusive's wait-queue wakeup. In the simulation both are models written by the author; in the eBPF version both are the kernel's own mechanisms, selected by how the listening sockets are set up rather than by any code of ours (Section 6.5).

FR6 (must). Implement a workload generator that reproduces the paper's four traffic regimes (high/low connection rate crossed with low/high per connection cost), including long lived connections and a mid-run worker stall, with traffic paced at a configurable rate.

FR7 (must). Record two metric streams for evaluation: per connection values (arrival, dequeue, and completion timestamps) and per loop iteration values (a WST snapshot, plus the scheduler's survivor counts at each filter stage and the resulting bitmap for Hermes). Trials must be seeded and reproducible.

FR8 (should). Implement a fifth scenario, beyond the paper's four, added because of a gap in the paper's own evaluation: its four regimes measure how evenly connections are dispatched, but not what an imbalance actually costs once it exists, since a worker holding many quiet long-lived connections pays no visible penalty until they become active. FR8 closes that gap with a synchronised burst of follow-up requests fired across accumulated long-lived connections, turning the rationale behind the Worker Status Table's connection-count metric from an assumption into a measurement.

FR9 (could). Replace the simulated dispatch path with a real eBPF program attached via SO_ATTACH_REUSEPORT_EBPF, real SO_REUSEPORT sockets, and a real epoll loop, driven by a separate load generator over a real network connection.

Non-functional requirements:

NFR1 (must). No locks anywhere, including the WST, the candidate map, and the accept queues. A lock is what normally keeps concurrent readers and writers of shared memory safe: a writer holds it exclusively, blocking every other worker's read or write of that data until it is released. That blocking is exactly what this design cannot afford, since the scheduler in every worker reads the entire WST at the end of every event-loop iteration, and under heavy load that can happen tens of thousands of times per second across just four workers; a lock contended that often would itself become the bottleneck the architecture exists to remove. The paper's alternative, and the one this requirement enforces, is to make each field individually atomic, safe to read or write in one indivisible hardware step without a lock, so a read can never observe a torn or corrupted value, while accepting that the three fields read together can be momentarily out of sync with each other, exactly as the paper allows.

NFR2 (must). The kernel-facing code must be written within eBPF's constraints (no heap allocation, statically bounded loops), so that the port to a real eBPF program is a mechanical swap rather than a rewrite. Section 7.3 reports how far this held in practice.

NFR3 (must). The full benchmark matrix and every figure and table in the dissertation must regenerate from a single command with the same seed.

NFR4 (should). Where either version has to deviate from the real system (for example the simulated processing cost, which both versions share), the deviation must be documented and its effect on the conclusions discussed (Sections 7.5, 8.7).

NFR5 (should). The simulation should use only the Rust standard library plus libc (for mmap, fork, waitpid). The eBPF version necessarily relaxes this, since loading and attaching a kernel program requires the Aya toolchain (Section 7.3).

Implementation language. The project is implemented in Rust rather than C, the language the original paper and most eBPF tooling use, and this is a requirement rather than a stylistic preference, for two reasons. First, NFR1's lock-free design pushes all of its correctness onto the individual atomicity of shared fields; Rust's ownership-based type system checks at compile time that shared memory is only ever touched through the atomic types this design requires, which C's compiler does not enforce, and a violation would otherwise corrupt the WST silently rather than fail loudly. Second, O5 (Section 1.3) targets a real eBPF program, and Aya, the Rust eBPF toolchain used for that objective, requires the whole codebase, kernel-facing and userspace code alike, to already be Rust; choosing it from the outset avoided a language rewrite between the two versions. Section 2.6 gives further background on eBPF and Rust's role in it.

4. Software Engineering Process

Three important factors shaped the engineering process of this project. Because it is a replication, the specification was fixed externally by the original hermes paper, and the targets the evaluation had to hit were known before any code was written. The code itself is concurrent, and therefore easy to get subtly wrong, particularly when building the userspace simulation. And because it is a single developer project on an 11 week dissertation timeline, both the process and the scope of the design had to fit what one person could deliver in this timeframe.

Development began with a close reading of the Hermes paper, from which a design document was written recording every architectural commitment the implementation would need to stick to: the three metrics and where each is updated, the filter order, θ, the bitmap encoding, the fallback rule, the lock-free rules, and the behaviour the paper expects in each traffic regime. This document became the project's requirements baseline, and Chapter 3 is derived from it. It also settled any ambiguity regarding where corners could be cut, if the paper specified something, the simulation had to match it, and if the paper left something open (such as the hang threshold constant) then the choice made was recorded and justified.

Because the requirements were fixed up front, the overall process was plan driven at the level of architecture: the split between kernel-facing and userspace components was decided before any code was written, specifically so that the eBPF version could later replace components without restructuring the system (Section 6.1). Within that fixed architecture, however, implementation itself was iterative rather than waterfall, and deliberately so. The work proceeded in three increments, each one only started once the previous was running end to end.

The first increment was a stripped-down preliminary simulator: four simulated cores running simple, non-atomic versions of the Hermes, LIFO, and reuseport dispatch algorithms, with no dispatcher component, no lock-free shared memory, and no metrics collection at all. Getting this skeleton working before any of the harder concurrency engineering was attempted surfaced the closed feedback loop's basic dynamics and the workload generator's pacing behaviour early and cheaply.

The second increment fleshed that skeleton out to the paper's full specification, one component at a time: shared memory first, then the WST, the scheduler, the dispatcher, the worker loop, the workload generator, and finally the benchmark harness, with unit tests written for each component before moving to the next. A fixed specification does not stop implementation problems from surfacing once real code exists (Section 7.5 records five of them). The test suite turns the paper's algorithms into executable checks: hang detection and θ behaviour for Algorithm 1, reciprocal_scale ranges, Nth-set-bit selection and the single or zero candidate fallback for Algorithm 2, the LIFO wait queue model, and the lock-free queue's wrap-around and overflow behaviour.

The third increment replaced the simulated kernel with the real one. This was deliberately left last, because it is the only part of the project that cannot be developed or tested on an ordinary development machine: it requires a running Linux kernel, administrator privileges, and code accepted by the kernel's safety checker. Leaving it until the architecture was settled and the evaluation harness already worked meant the port could be judged on its own terms, against a set of results that already existed, rather than being debugged at the same time as the design. That ordering paid off directly, since the components that were designed to carry over unchanged did carry over unchanged (Section 7.3).

Testing the system as a whole, rather than its parts, was benchmark-driven. The rankings the paper expects in each traffic regime were written down as explicit validation targets before the benchmarks were run, and the analysis pipeline checks them automatically and prints a pass or fail verdict for each (Section 8.1). Because those targets were fixed in advance and the same analysis pipeline was later pointed at the eBPF version's results, one of them is now recorded as failing rather than passing; it is reported as such in Appendix A rather than being retuned after the fact. Git was used throughout, hosted on GitHub, with the analysis notebook, benchmark scripts and results all kept in the repository so that the whole evaluation can be reproduced from a clean checkout. Five deviations found during development are reported in Section 7.5 rather than being quietly fixed and left undocumented.

5. Ethics

The project raises no ethical concerns requiring approval. It involves no human participants, no user studies, no personal data, and no deployment against real traffic: all benchmark traffic is synthetic, generated and consumed within a single machine. The artefact evaluated is the author's own code. The system replicated is described in a peer-reviewed publication (Pan et al., 2025); no proprietary code, data, or confidential material from Alibaba was used or available — the reimplementation works exclusively from the published paper, which is standard and encouraged research practice (independent replication). The completed ethics self-assessment form is included as an appendix.

6. Design

6.1 System Architecture Overview

Micro-Hermes's architecture is split deliberately along the kernel/userspace boundary, and this chapter describes the design that both versions share. The reason one design covers both is that the boundary was chosen up front to fall exactly where the real system's own boundary falls. Everything that lives in userspace in the real system, the Worker Status Table, the per-worker scheduler, and the logic of the connection dispatcher, is written once and used unchanged by both versions. Only the things that genuinely belong to the operating system differ: where the connections come from, how a worker waits for them, and where the dispatcher's decision is actually executed.

The two versions therefore differ not in what they do but in what is real:

- **The simulation** replaces the operating system with a parent process. It generates synthetic connection arrivals at a configurable rate, computes a number standing in for the identifier the kernel would derive from a connection's addresses and ports, runs the dispatch mechanism under test, and places each connection into the chosen worker's queue. Four forked child processes stand in for the workers. The Worker Status Table, the candidate bitmap, and the queues all live in one region of shared memory mapped before forking.
- **The eBPF version** removes the stand-ins. Connections arrive over a real network port from a separate load-generator process; each worker owns a real listening socket and waits on it with the kernel's own event-notification interface; and the dispatch decision is made by a small program running inside the kernel itself, consulted by the kernel each time a new connection completes its handshake.

What is worth stressing, because it is the payoff of the design decision rather than an accident, is how little had to change. The status table, the scheduler and its filters, the dispatcher's selection logic, and the definitions of all five traffic scenarios are identical in both versions. This is a direct consequence of NFR2: the dispatcher was written from the start under the restrictions the kernel imposes on programs it will accept, so it could be moved into the kernel rather than rewritten for it.

In both versions the feedback loop is closed: a worker's load comes entirely from the connections the dispatcher assigned to it, and the dispatcher's decisions are driven by the bitmap the workers' schedulers publish. Scheduling decisions therefore directly shape the measured load distribution, which is exactly what the evaluation sets out to test. Figure 1 shows both layouts side by side.

[FIGURE 1 HERE — side-by-side architecture diagram, one panel per version, drawn so the shared parts line up horizontally and the differences are visually obvious.

LEFT PANEL, "simulation": top box "parent process (stands in for the kernel)" containing generator (paces arrivals at the workload's connection rate) → dispatcher (Algorithm 2 over M_Sel, or the reuseport-hash / LIFO model) → pushes into the chosen worker's queue. Bottom box "4 forked worker processes" running the instrumented event loop (stamp time → collect batch → process events → update WST), with the scheduler (Algorithm 1) at loop end writing the candidate bitmap to M_Sel. Middle strip "shared memory": WST (one cache-line slot per worker) + M_Sel + one queue per worker.

RIGHT PANEL, "eBPF version": separate box on the left for "load generator process", connected by an arrow labelled "real TCP connections" crossing into a box labelled "kernel", which contains the eBPF program reading M_Sel and selecting a socket. Below it, "4 forked worker processes", each owning its own real listening socket and its own epoll instance, running the same instrumented event loop, with the same scheduler at loop end — but now writing M_Sel through a system call into a kernel map rather than a plain memory store. Shared memory strip still carries the WST.

Shade or colour the components that are byte-identical across the two panels (WST, scheduler, dispatcher logic, event loop) to make the point that only the outer ring changes.

Caption: The two implementations side by side. The Worker Status Table, the scheduler and its cascading filters, and the dispatcher's selection logic are the same code in both. What differs is everything around them: in the simulation a parent process stands in for the kernel and connections are synthetic; in the eBPF version connections arrive over a real network port, each worker owns a real socket, and the dispatch decision is executed by a program running inside the kernel.]

6.2 Worker Status Table

The WST holds, per worker, the three status metrics of Pan et al. (2025), each stored as an individually atomic 64-bit integer. The paper draws the table as one row per metric spanning all workers; this implementation transposes that layout into one slot per worker, which changes nothing about who reads or writes which field. The metrics are the timestamp of the most recent event-loop entry (used for hang detection), the count of pending events (events delivered by epoll_wait but not yet handled, a proxy for instantaneous processing load), and the accumulated count of open connections (a guard against future overload from synchronised bursts on long-lived connections). Each slot is padded to a 64-byte cache line to prevent false sharing between workers on adjacent cores.

The concurrency design is preserved exactly: the table is partitioned by writer, with each worker updating only its own slot, and the scheduler reads the entire table without locks. Reads may race with updates from other workers; as in the original system this is accepted by design, since per-field atomicity prevents torn values and a marginally stale metric does not meaningfully change a scheduling decision.

6.3 Kernel Connection Dispatcher

The dispatcher implements Hermes's Algorithm 2. For each new connection it reads the candidate bitmap from M_Sel, counts the set bits, and, if more than one candidate survives, scales the connection's hash into the candidate count using the Linux kernel's reciprocal_scale function (a multiply and shift mapping, reimplemented exactly) and selects the Nth set bit of the bitmap. If one or zero candidates survive, it falls back to plain reuseport hashing across all workers, matching the paper's fallback rule: dispatching every connection that arrives between scheduler updates to a single "best" worker would itself overload it.

This logic is identical in both versions; only where it runs changes. In the simulation it executes in the parent process, and M_Sel is a single shared atomic 64-bit integer. In the eBPF version the same selection runs inside the kernel, and M_Sel is a kernel-held table of one entry that both sides can read and write. The reason the same code serves both is NFR2: it was written from the start without heap allocation and with loops whose length is fixed at compile time, because those are the conditions the kernel imposes on any program it will accept. Section 7.3 reports what this cost in practice, and where the port was less mechanical than intended.

6.4 Userspace Scheduler

Every worker runs Hermes's Algorithm 1 at the end of each event-loop iteration: there is no dedicated scheduler process, exactly as in the original design. The reason is the one established in Section 2.5: a dedicated dispatcher or scheduler process on the connection path becomes a bottleneck at high connection rates and a single point of failure, whereas embedding the scheduler in every worker means scheduling continues as long as *any* worker is alive — a property the evaluation observes directly when a stalled worker is excluded by its peers' schedulers (Section 8.4). The scheduler snapshots the WST and applies three cascading filters in the paper's fixed priority order. The time filter removes workers whose last loop entry is older than a hang threshold (200 ms in this implementation, the paper leaves the constant implementation-defined). The connection-count filter and the pending-event filter each remove workers whose metric exceeds the candidate average plus an offset theta, which prevents the candidate set from collapsing when workers are near-uniformly loaded. Following the paper's empirical finding that theta/avg = 0.5 is optimal, theta is computed as half the candidate average, with a small floor so the filter remains permissive at cold start. The surviving set is encoded as a bitmap and published to M_Sel.

Because the scheduler runs once per loop iteration and epoll_wait is bounded by a 5 ms timeout, every worker is guaranteed to re-enter its loop, and therefore to refresh its timestamp and re-run the scheduler, at least once every 5 ms even with no traffic at all, keeping both hang detection and the candidate set live under idle conditions.

One consequence of this design is invisible in the simulation but significant in the eBPF version, and it becomes one of the more interesting findings of the evaluation. Publishing the bitmap is nearly free when M_Sel is just a variable in shared memory: it is a single machine instruction. Publishing it into a table the kernel owns is not free, because crossing from an ordinary program into the kernel requires a system call, which is orders of magnitude more expensive. Since the scheduler runs at the end of *every* loop iteration, the cost of publishing scales with how often the loop turns over, which in turn scales with how fast connections arrive. Under heavy traffic of cheap connections this becomes the dominant cost of the whole mechanism, which is exactly what Section 8.3 measures and Section 8.6 explains.

6.5 The Two Baseline Mechanisms

Micro-Hermes is compared against the two standard Linux mechanisms described in Section 2.4. This is the part of the design where the two versions differ most sharply, and the difference matters enough to the credibility of the whole evaluation to be worth stating plainly: in the simulation, both baselines are models written by the author; in the eBPF version, both are the operating system's own behaviour, which the author does not implement at all.

In the simulation, the reuseport baseline applies the same scaled-hash selection across all workers unconditionally, reproducing the stateless behaviour of the real mechanism: no awareness of worker state, and no scheduler runs at all. The epoll-exclusive baseline models the kernel's wakeup rule rather than a caricature of it. In the real kernel, the waiting workers form a queue ordered by when each registered its interest, with each new registration going to the front, so the order is fixed once startup is done and the last worker to register sits permanently at the head. An arriving connection wakes the first idle worker found scanning from that head. The simulation reproduces this using state the generator can see: registration order is fork order, so the highest-numbered worker is the head; a worker counts as idle when its queue is empty and it has no pending events; and when no worker is idle the connection goes to whichever has the shortest backlog, approximating the shared queue that the next free worker would drain.

That last clause is the weak point, and the simulation's evaluation flagged it as such (Section 8.7). Choosing the shortest backlog is a load-aware decision, and the real mechanism cannot make it, because under epoll exclusive there are no per-worker backlogs to compare: every worker shares one socket and one queue.

The eBPF version removes the modelling entirely, and it does so without implementing either baseline, by changing only how the listening sockets are set up:

- For **reuseport** and **Hermes**, each worker opens its own listening socket on the shared port. The kernel then decides which socket receives each connection: by its own internal hash for the reuseport baseline, or by consulting our eBPF program for Hermes.
- For **epoll exclusive**, there is one socket, shared by all four workers, each registering interest in it with the flag that asks the kernel to wake only one waiting worker per arrival. Everything after that is the kernel's own wait-queue behaviour.

The consequence is that the baseline numbers in Section 8.3 are measurements of Linux, not of the author's understanding of Linux, and the specific concern raised above disappears: there are genuinely no per-worker backlogs for the exclusive baseline to consult, because the kernel's design does not have them. Section 8.6 reports what happened to the simulation's conclusions once this was tested directly, and one of them does not survive.

In both versions, every worker runs the paper's 5 ms event-loop timeout under every policy, so no policy gets an artificial scheduling advantage from a different wakeup cadence (Section 7.5 explains why this is safe for the exclusive baseline).

7. Implementation

This chapter covers both implementations. Section 7.1 describes what they share, since that is most of the system. Sections 7.2 and 7.3 then describe what is specific to each. Section 7.4 records one property they share that is important to state openly, because it bounds what either can claim, and Section 7.5 records the problems encountered along the way.

7.1 Shared Foundations

Both versions are written in Rust, for the reasons given in Chapter 3, and both take the same components unchanged: the Worker Status Table, the scheduler and its three cascading filters, the dispatcher's selection logic, and the definitions of all five traffic scenarios. Together these are the substance of the architecture being replicated; what changes between versions is the machinery around them.

The simulation is about 1,750 lines across nine modules with a single dependency (libc, for mmap, fork and waitpid). The eBPF version adds roughly 2,100 lines across four crates: the program that runs inside the kernel, a small crate of definitions shared between kernel and userspace code, the load-balancer process that sets everything up and runs the workers, and a separate load generator. Unit tests cover the scheduler's filters, the dispatcher's bit manipulation, and the shared-memory queue; the scheduler's tests carry over to the eBPF version unmodified, because the scheduler itself did.

In both versions the Worker Status Table lives in memory shared between processes, mapped before the workers are forked so that parent and children address the same physical pages without any message-passing machinery. Each worker writes only its own slot, and each slot is padded to a cache line so that two workers updating their own status do not contend for the same piece of memory.

7.2 The Simulation

All cross-process state lives in one structure mapped with mmap(MAP_SHARED | MAP_ANONYMOUS) before forking, so atomic operations on the shared pages are well-defined across the process boundary. Alongside the WST and M_Sel, the region contains one single-producer single-consumer ring buffer per worker, standing in for the queue the kernel would keep for each listening socket. Because each ring has exactly one producer (the generator) and one consumer (the owning worker), acquire/release ordering on the head and tail indices is sufficient and the design remains lock-free throughout, mirroring the WST's partitioned-writer principle. A full ring rejects the connection, modelling the overflow that occurs when a real worker cannot accept quickly enough.

The parent process generates connections at the workload's target rate. Rather than sleeping a fixed gap between one arrival and the next, it pins each arrival to an absolute point on the clock and sleeps until that time; this way, if any one sleep overshoots slightly, the error does not build up over the run and the overall rate stays on target.

Each connection is given three properties when it is created. The first is a synthetic identifier standing in for the value the kernel would derive from a connection's addresses and ports; here it is produced by scrambling a simple counter with a fixed multiplicative constant, and an optional per-trial seed varies the sequence between benchmark runs. The second is a processing cost, sampled from a distribution. The third is a lifetime, after which the worker that received the connection closes it.

7.3 The eBPF Implementation

The eBPF version replaces every stand-in with the real thing. Three pieces of operating-system machinery do the work, and because they are the least familiar part of this dissertation they are worth describing in plain terms before the detail.

The first is a way to run custom code inside the kernel safely. Ordinarily, code that runs inside an operating system kernel can crash the whole machine, which is why kernels do not accept code from applications. eBPF is the mechanism that makes it possible anyway: a program is submitted to the kernel, which checks it before allowing it to run at all, rejecting anything it cannot prove will finish and will only touch memory it is entitled to touch. A program that passes is compiled and attached to a specific decision point. This is why the restrictions of NFR2 exist, and why they had to be respected from the first line of the dispatcher rather than retrofitted.

The second is a way for the kernel program and ordinary programs to share data. eBPF provides small typed tables, conventionally called maps, that both sides can read and write. Micro-Hermes uses two: one holding the single number that encodes the candidate bitmap, and one holding the mapping from worker number to that worker's listening socket. The first is written by the workers' schedulers and read by the kernel program on every connection; the second is filled in once at startup and never changed. Both are pinned to a filesystem path, which simply means the kernel keeps them alive independently of the process that created them, so the setup process can exit while the workers keep using them.

The third is the decision point itself. When several sockets share a port, the kernel normally picks between them with a fixed internal calculation. The socket option this project uses replaces that fixed rule with a question put to our program: given this connection, which worker should take it? The program reads the bitmap, counts how many workers are currently acceptable, scales the connection's identifier into that count, and names the corresponding worker.

Two details of the port are worth recording because they show the design boundary holding, or not, under contact with reality. The fallback rule turned out to need no code at all: the paper specifies that when fewer than two candidates survive the filters, dispatch should fall back to the kernel's ordinary behaviour, and in this interface a program that simply declines to choose gets exactly that, since the kernel then applies its own rule. What the simulation implemented as an explicit branch became, in the real system, the absence of one. Conversely, the injected worker stall could not carry over as written. In the simulation the load generator knew which worker would receive a connection and could stall that worker directly; in the eBPF version the generator is a separate process on the far side of a network connection and has no way to reach inside the load balancer, so the stall is instead requested of the load-balancer process itself at startup, which applies it to the nominated worker at the nominated time. The parameters are unchanged: worker 0, stalled for 400 ms, starting 1.5 seconds in.

The load generator is a separate program that opens real connections to the load balancer's port and sends each request over the wire, carrying the processing cost that request should incur, and timing the round trip itself. Measuring from the client rather than inside the server is deliberate: it is what a real caller of the load balancer experiences, and it includes every cost along the path rather than only the part the server chooses to count. Section 8.1 explains why this makes the two versions' latency figures non-interchangeable.

7.4 What Both Versions Simulate

One thing is not real in either version, and it bounds what either can claim. In both, a worker "processes" a connection by sleeping for that connection's assigned cost rather than performing actual Layer 7 work. Cost is therefore a property of the connection rather than of the worker, which does match how the paper describes real Layer 7 work, where expense depends on what a connection involves, a TLS handshake, compression, a protocol translation, and varies from one connection to the next. In the eBPF version the cost is chosen by the load generator and sent with the request, so the server sleeps for a duration the client selected.

This keeps the traffic model identical across both versions and across all fifteen benchmark points, which is what makes the comparison in Section 8.6 meaningful. The cost is that neither version can be used to reproduce the paper's CPU-overhead measurements, since the processor is idle during the sleep rather than doing work that would appear in a profile. The dispatch path in the eBPF version is real and its cost is real, and Section 8.3 shows that cost appearing in the results; but the workload it is dispatching is not. This limitation is carried forward explicitly to Section 9.2.

7.5 Engineering Challenges

Building the preliminary simulator described in Chapter 4 before the full specification paid off directly: because the closed feedback loop and the generator's pacing were already validated against a trivial, non-atomic version of the system, the issues below could be isolated to the engineering added afterwards rather than tangled up with basic questions about whether the loop worked at all.

First, metrics collection across fork(): an early design accumulated benchmark records in a mutex-protected vector placed in the shared region, which is unsound; a vector's heap allocation is not in the shared mapping, and standard-library mutexes are undefined across processes on some platforms (on macOS the underlying os_unfair_lock aborts with EINVAL). Each worker instead buffers records privately and writes its own file shard, which the parent merges after waitpid, applying the WST's single-writer partitioning idea to the benchmarking plumbing itself.

Second, a subtle fidelity artefact in the simulation's epoll-exclusive baseline. An early version modelled the wait-queue head dynamically, as the worker with the most recent loop-entry timestamp, and gave idle baseline workers a periodic housekeeping timer. Because the workers are forked simultaneously, their timers were phase-locked: each expiry re-stamped a different worker in lock-step, rotating the concentration target about once a second and producing per-worker totals that were artificially near-balanced, masking the very pathology the baseline exists to demonstrate. The fix was to model what the kernel actually does: the queue order is fixed at registration time, with the last-registered worker permanently at the head. With a static order, idle wakeups no longer reshuffle priority, so every policy can safely share the paper's 5 ms timer, and the baseline exhibits the persistent concentration the real mechanism shows. The eBPF version retires this problem entirely, since it does not model the mechanism at all.

Third, batch sizing in the simulation. Real Layer 7 events cost microseconds, but the simulated per-event sleeps cost milliseconds, so a conventional batch limit would stretch a single loop iteration past the hang-detection threshold and starve the scheduler of fresh status data. The simulation's limit is therefore kept small, at 4 events, preserving the paper's property that scheduler frequency scales with load. The eBPF version retained the conventional limit of 64, which is the more realistic choice for a real event loop but means that, because it also simulates processing by sleeping, it can spend far longer inside one iteration when events are expensive. This difference between the two versions is carried into the evaluation's methodology (Section 8.1), since it affects the per-iteration measurements in Sections 8.4 and 8.5 without affecting the per-connection ones.

Fourth, connection lifetime in the eBPF version. Cases 3 and 5 describe connections that stay open for 60 seconds, which in the simulation simply meant they never closed during a 4-second run. Taken literally by a real client holding a real socket, it would have meant every benchmark point blocking for a full minute after its traffic finished. The generator therefore caps how long it holds a connection at the run's duration plus a short grace period, which preserves the intent, that no connection closes while the run is in progress, without stalling an automated matrix of 117 runs.

Fifth, the cost of publishing status. This did not appear as a defect but as a result, and it is the clearest example of something the simulation could not have shown. Writing the candidate bitmap is a single memory store in the simulation and a system call in the eBPF version, and because the scheduler runs at the end of every loop iteration, that cost is paid at whatever rate the loop turns over. Under Case 1 at heavy load this is the dominant cost of the entire mechanism, and it inverts the ranking (Sections 8.3 and 8.6). No adjustment was made in response; the result is reported as measured.

8. Evaluation and Critical Appraisal

This chapter evaluates both versions. Section 8.1 sets out the method, which is common to both, and explains where the two benchmarks differ and what that means for reading their numbers together. Sections 8.2 and 8.3 report each version's results separately. Sections 8.4 and 8.5 examine the two mechanisms Hermes depends on, hang detection and the cascading filter, in both versions. Section 8.6 then compares the two directly, which is where the most interesting findings are, and Section 8.7 discusses what the whole exercise does and does not establish.

8.1 Evaluation Framework and Methodology

The evaluation reproduces the four traffic regimes used by Pan et al. (2025), characterised by connections-per-second (CPS) and per-connection processing cost: Case 1, high CPS with low cost (a stress or traffic-spike scenario); Case 2, high CPS with high cost (compression-heavy traffic, configured here as a sustained overload at roughly 112% of aggregate worker capacity, with a 400 ms stall injected into one worker); Case 3, low CPS with low cost but long-lived connections that never close within the run (the finance/chat pattern, and the most common case in Alibaba's production); and Case 4, low CPS with high cost (TLS- and regex-heavy web services). A fifth scenario, beyond the paper's four, repeats Case 3's accumulation of long-lived connections and then fires a follow-up request on every open connection simultaneously, providing direct evidence for the cost of connection concentration (Section 8.2, Case 5).

Following the paper's Table 3 methodology, each of the four cases is additionally swept across three offered-load levels — light, medium and heavy — by scaling the case's connection arrival rate while holding its cost distribution fixed. Utilisation (offered load divided by the four workers' aggregate service capacity) spans roughly 10% to 75% for Case 1, 45% to 112% for Case 2 (its heavy level is a sustained overload; the 400 ms stall is part of the case's profile and is injected at every level), and 28% to 93% for Case 4; Case 3's cost is trivial, so its levels instead scale how many long-lived connections accumulate (120, 240 and 600 per run). The detailed per-case analysis in Sections 8.2 and 8.3 uses each case's *characteristic* level — the level at which the scenario's defining behaviour is clearest (light for Case 1, where sub-capacity load exposes the concentration pathology in its purest form; heavy for Case 2, whose defining feature is the overload; medium for Cases 3 and 4) — and the full sweep is then reported separately for each version (Tables 2 and 4) to test whether the rankings hold as load varies, which is where the paper's own rankings are defined. This convention applies identically to both versions, so the two are always compared at the same operating point.

Each of the three dispatch policies is run against every case and load level three times, with a per-trial seed varying the (reproducible) sequence of connection identifiers and sampled processing costs. Two record streams are collected: one row per completed connection, and one row per worker event-loop iteration (a WST snapshot plus, for Micro-Hermes, the scheduler's per-stage survivor counts and bitmap). Three metrics are derived, matching the paper's: latency (mean and 99th percentile; percentiles are computed within each trial and then aggregated across trials), throughput (completed connections per second, informative once offered load approaches or exceeds capacity), and load balance (the standard deviation of per-worker connection counts, the metric the Hermes paper uses for its production comparison). The full pipeline, from benchmark execution to every figure and table referenced below, is reproducible from a single annotated Jupyter notebook in the project repository.

How the two benchmarks differ. Both versions were run over exactly the same experiment: the same five cases, the same three load levels, the same connection rates, the same cost distributions and connection lifetimes, the same injected stall, three trials each, and the same random seeds driving the same cost sequences. This was deliberate, and it is what makes Section 8.6's comparison possible at all. But the two harnesses are not the same machinery, and three differences matter when reading their numbers side by side.

The first and most important is what "latency" measures. In the simulation, everything happens inside one process tree: the generator hands a connection to a worker through shared memory, and latency is the time from that handover to the worker finishing the work. There is no network, no operating-system socket, and no connection setup in that measurement. In the eBPF version, latency is measured by the load generator, which is a separate process: it is the time from sending a request over a real connection to receiving the reply. That figure necessarily includes costs the simulation has none of, among them establishing the connection, moving data through the kernel's networking stack, and the operating system deciding when to run each process. The eBPF version's latencies are therefore not merely different numbers for the same quantity; they are a larger quantity, measured from further away. Client-side measurement was chosen because it is what a real user of a load balancer actually experiences.

The second is that the eBPF version can fail in ways the simulation cannot. A real connection can be refused or reset; the simulation's equivalent was a full queue rejecting a connection. These outcomes are recorded but carry no latency, and are excluded from latency statistics.

The third concerns Case 5. In the simulation, the burst fires at 2.5 seconds and reaches whichever connections happen to be open at that moment, roughly 150 of them. In the eBPF version the generator holds its connections explicitly and fires on all of them, 240. The burst is therefore measured over a larger population in the eBPF version, so the two Case 5 medians describe differently sized events and should not be read as one number improving on the other.

The practical consequence, applied throughout this chapter: the two versions may be compared on rankings, on how policies behave relative to each other, and on trends as load rises. They may not be compared by putting one version's millisecond figure next to the other's and treating the difference as an improvement or a regression.

One simplification shared by both versions, and important enough to state before any results are read. Both listen on a single port. The system being replicated does not: a production Layer 7 load balancer separates tenants by port and listens on roughly ten thousand of them (Section 2.1). This matters specifically for the epoll-exclusive baseline, because the cost of that mechanism grows with the number of ports while the other two policies' does not. Pan et al. measure this directly and give it as one of the two reasons epoll exclusive performs poorly in their Case 1: dispatching a new connection costs them O(1) under Hermes and reuseport but O(#ports) under exclusive, since every worker registers interest in every port. With a single port that cost is at its minimum, so both versions here test epoll exclusive in the most favourable configuration it can have. Wherever the results below show epoll exclusive performing well, this should be read as a lower bound on its true cost rather than a measurement of it, and Section 8.6 returns to what that does and does not allow this project to conclude.

A second shared simplification, noted here because it affects how the two versions' internal measurements line up: the simulation collects at most 4 events per loop iteration where the eBPF version collects up to 64. The simulation's limit was deliberately made small, for the reason given in Section 7.5, so that its millisecond-scale simulated processing could not stretch one iteration past the hang threshold. The eBPF version kept the conventional larger limit. Since both versions simulate processing by sleeping, the eBPF version can therefore spend considerably longer inside a single iteration when events are expensive, which is visible in its hang-detection trace (Section 8.4) and contributes to its sharper filter pruning (Section 8.5). It does not affect the latency or balance figures, which are measured per connection rather than per iteration.

[TABLE 1 HERE — summary statistics for the simulation (regenerate from analysis/results; the committed summary_stats.tex now holds the eBPF version's figures, which appear as Table 3).

Caption: Simulation: latency and balance per dispatch policy across all five scenarios at each case's characteristic load level (mean over 3 trials; p99 shown ± sd across trials). Case 5 rows are burst follow-up requests, measured from the burst instant.]

8.2 Results: The Simulation

This section reports the simulation's results. Throughout it, the epoll-exclusive baseline is a model written by the author rather than the kernel's own behaviour, which matters for interpreting it and is picked up in Sections 8.6 and 8.7.

Table 1 summarises latency and balance for every scenario; Figure 2 shows the full latency distributions (one panel per case) and Figure 3 isolates the 99th percentile with its trial-to-trial spread. The following paragraphs walk through the cases in turn.

[FIGURE 2 HERE — latency distributions, simulation (regenerate from analysis/results)

Caption: Simulation: latency distributions (queueing + processing) per dispatch policy, one panel per workload case at its characteristic load level, log x-axis, trials pooled. In the light-load cases (1 and 3) the three policies are indistinguishable. Under load (Cases 2 and 4) reuseport's blind hash produces a heavy tail — it keeps assigning connections to busy or stalled workers — while Micro-Hermes routes around them.]

[FIGURE 3 HERE — 99th-percentile bars, simulation (regenerate from analysis/results)

Caption: Simulation: 99th-percentile connection latency per policy (bars: mean of per-trial p99; error bars: min–max across trials; note the independent y-scales). Under load, reuseport's p99 exceeds Micro-Hermes's — by roughly 30% in Case 2 and 2.2x in Case 4. The LIFO model's low latency in those cases is discussed in Section 8.7, and Figure 6 shows the liability that accompanies it.]

Case 1 (high CPS, low cost). When requests are cheap and the system is far below capacity, all three policies deliver essentially the same latency: every policy completes a typical connection in 1.3 ms. The mechanisms differ sharply in how fairly they spread the work. Across the 1,200 connections of a run, Micro-Hermes distributes most evenly (a standard deviation of 14 connections between workers, against 29 for reuseport), while the epoll-exclusive model sends every single connection to the wait-queue head worker (standard deviation 520): because each request is processed faster than the next one arrives, the head worker is always idle again in time to be woken next, and the other three workers never receive anything. Micro-Hermes's tail is marginally higher than the baselines' (99th percentile 1.9 ms against 1.4 ms). The cause is structural rather than incidental: Stage 3 never picks the single best worker for a connection, only hashes it to one of a subset of roughly acceptable candidates that Stage 2 handed down. In one trial, about 0.6% of connections happened to hash to a candidate that was momentarily busier than some other worker excluded from that subset, and briefly queued behind it. That handful of connections is the price of the two-stage design - restricting the kernel's hash to a subset, rather than letting it pick freely among all workers, occasionally rules out the least-loaded one. This is consistent with the paper's own ranking for this regime, which places plain reuseport marginally ahead of Hermes when load is trivially light, precisely because Hermes's worker-awareness has nothing to protect against yet and its candidate-subset overhead shows up as pure cost.

Case 2 (high CPS, high cost, with an injected hang). This case pushes the offered load slightly past what the four workers can process, so queues grow throughout the run and latency is dominated by waiting. Here awareness of worker status pays off directly: Micro-Hermes achieves a mean latency of 570 ms against reuseport's 784 ms (27% lower), and a 99th percentile of 1,579 ms against 2,037 ms. The gap has a simple cause: reuseport's hash does not know that a worker is busy or stalled, so it keeps sending roughly a quarter of all new connections to the stalled worker for the full duration of the stall, while Micro-Hermes routes around it. The epoll-exclusive model posts the lowest latency of all (mean 414 ms); Section 8.7 explains why this was read, at the time, as a boundary of the simulation rather than a property of the real mechanism, and Section 8.6 reports what the eBPF version had to say about that reading.

Case 3 (low CPS, long-lived connections). This case reproduces the paper's headline production result. Because connections never close within the run, every dispatch decision is permanent: mistakes accumulate over time, and the balance of open connections at the end of the run exposes each mechanism's character. Figure 4 shows that balance as a trajectory over the run, and Figure 5 shows who actually ends up holding the work. The epoll-exclusive model ends with a standard deviation of 104 connections - the head worker holds all 240 connections of the run while the other three hold none - against 15 for Micro-Hermes and 14 for reuseport. This is the same ordering the paper reports from production (standard deviations of 3,200, 50 and 20 for exclusive, reuseport and Hermes respectively). Micro-Hermes matches rather than beats reuseport here; Section 8.7 discusses why parity is the faithful expectation in this synthetic setting.

[FIGURE 4 HERE — balance over time, simulation (regenerate from analysis/results)

Caption: Simulation: standard deviation of per-worker open-connection counts during a Case 3 run (long-lived connections; mean over trials, bands span min–max). Micro-Hermes and reuseport hold the imbalance near-flat as connections accumulate. Under the LIFO model the imbalance grows linearly: at this low arrival rate the wait-queue head worker is always idle again before the next connection arrives, so it receives every one.]

[FIGURE 5 HERE — concentration profile, simulation (regenerate from analysis/results)

Caption: Simulation: connections handled per worker in Case 3, ranked busiest to least busy within each trial (bars: mean over trials; error bars: min–max; dashed line: even share). Under the LIFO model the head worker takes all 240 connections in every trial and the remaining three take none. Micro-Hermes and reuseport sit close to the even share at every rank.]

Case 4 (low CPS, high cost). With expensive requests at moderate utilisation (about 75%), the danger is queueing a new connection behind a slow worker which is already busy. Micro-Hermes's 99th-percentile latency of 442 ms is less than half of reuseport's 975 ms as it avoids doing this. Reuseport, as it hashes blindly, cannot. The epoll-exclusive model again leads (295 ms), with Micro-Hermes tracking it as the paper predicts: Hermes's status detection reacts with a small delay where exclusive reacts immediately.

Case 5 (synchronised burst on long term connections). Cases 1 to 4 measure the latency of new connections, which cannot show why concentration is dangerous: a worker hoarding idle connections pays no visible penalty while they stay idle. Case 5 makes the danger measurable and shows why the thus far top performing LIFO is insufficient. It repeats Case 3's accumulation - by 2.5 seconds each policy has dispatched roughly 150 long-lived connections - and then every open connection fires one 5 ms follow-up request at the same instant, modelling the synchronised bursts (a market opening for finance traffic, a mass push notification for chat) that the WST's connection count metric exists to guard against. Because a request on an established connection can only be processed by the worker that owns it, the burst is processed exactly as unevenly as the connections were dispatched. Under the epoll-exclusive model the head worker owns all of the connections and must work through the entire backlog alone: the median follow-up waits 405 ms and the 99th percentile 801 ms. Micro-Hermes and reuseport, having spread the same connections across all four workers, complete the same burst with a median of 107 ms and a 99th percentile of roughly 250 ms (Figure 6). This is the measured form of the trade-off the raw latency numbers of Cases 2 and 4 conceal: the epoll-exclusive model's fast dispatch coexists with a standing liability that comes due the moment its hoarded connections wake up.

[FIGURE 6 HERE — burst latency, simulation (regenerate from analysis/results)

Caption: Simulation: latency of follow-up requests when every open connection fires one simultaneously (Case 5, ~150 open connections at burst time, trials pooled, measured from the burst instant). Under the LIFO model one worker owns every connection and must serialise the entire burst — the straight-diagonal CDF is the signature of a single queue draining at a constant rate — while Micro-Hermes and reuseport, having spread the same connections across all four workers, complete it 3–4x faster.]

Across all five cases Micro-Hermes never collapses: where it is not the best policy it trails the best by a fraction of a millisecond (Case 1) or a small margin (Cases 2 and 4), whereas each baseline fails badly in at least one regime - epoll-exclusive on balance in Cases 1 and 3 and on burst latency in Case 5, reuseport on latency in Cases 2 and 4. This adaptability, rather than outright victory in any single regime, is the core property claimed for the Hermes architecture, and the simulation reproduces it.

Load sensitivity. The paper's rankings are defined across a sweep of offered load, not at a single point, so Table 2 repeats the comparison at light, medium and heavy load for each of the four cases, reporting the paper's three metrics (mean latency, P99, throughput).

[TABLE 2 HERE — load sweep, simulation (regenerate from analysis/results; the committed load_sweep.tex now holds the eBPF version's sweep, which appears as Table 4)

Caption: Simulation: the Table-3-style load sweep — mean latency, P99 and throughput per policy at light/medium/heavy offered load in each case (mean over 3 trials). Every Case 2 level carries the injected stall.]

The sweep confirms that the rankings above are not artefacts of one operating point, and reproduces the paper's central load-dependence claim: the value of worker-awareness grows with load. In Case 1, where all three policies are indistinguishable at light and medium load, heavy load (75% utilisation) separates them: reuseport's mean latency rises to 13.4 ms and its 99th percentile to 33 ms as hash collisions land connections behind busy workers, while Micro-Hermes holds 3.7 ms and 24 ms — reproducing the paper's ranking flip for this case, where reuseport is marginally ahead of Hermes at light load but behind it once load is heavy. In Cases 2 and 4 the Micro-Hermes-over-reuseport gap widens monotonically with load: Case 4's mean-latency ratio grows from 1.2x at light load to 2.5x at medium and 2.8x at heavy, with the 99th percentile reaching 638 ms against 1,864 ms. Throughput tells the same story from the other side: once offered load approaches capacity, reuseport can no longer even sustain the arrival rate — at Case 2's medium level (84% utilisation) it completes 59 connections/s of the 75 offered where Micro-Hermes completes 69, and at Case 4's heavy level it completes 35/s against Micro-Hermes's 45 — because connections hashed onto overloaded workers sit in queues rather than completing. Case 3 shows latency parity at every level, as expected: its per-connection cost is trivial and its failure mode is balance, not latency, which Figures 4 and 5 already capture. The epoll-exclusive model posts the lowest raw latency at every level of Cases 2 and 4, and the simulation attributed this to its own modelling shortcut, since its fallback path reads per-worker backlogs that the real mechanism does not have (Section 8.7). On that reading its latency column should be treated as an optimistic bound rather than a property of real epoll exclusive. Section 8.6 tests that attribution against the real mechanism, and the answer is not the one the simulation expected.

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

Case 1 (high CPS, low cost). At light and medium load the three policies are again close, with Micro-Hermes marginally ahead on both mean and tail (1.2 ms and 1.9 ms at light, against 1.3/2.3 for reuseport). Balance behaves as the architecture predicts: across the 1,200 connections of a run Micro-Hermes spreads most evenly (standard deviation 13.7 against reuseport's 14.5), while epoll exclusive concentrates severely (460). The real mechanism therefore shows the same pathology the simulation's model showed, which is the clearest single confirmation that the model was capturing something real.

At heavy load, however, the ranking inverts, and this is the most striking result in the chapter. Micro-Hermes's mean latency rises to 26.5 ms while epoll exclusive holds 3.0 ms and reuseport 5.1 ms, with all three sustaining essentially the offered rate (2,971 to 2,998 connections/s). Micro-Hermes is roughly nine times slower than the better baseline on a workload where it had been the best performer moments earlier at medium load.

This result contradicts the paper, and the contradiction is worth stating plainly rather than presenting the finding as merely novel. Pan et al. report Hermes as the *best* performer in exactly this cell: at Case 1 heavy their measured means are 5.02 ms for Hermes, 5.10 ms for reuseport and 7.09 ms for epoll exclusive. This project measures the opposite ordering. Two differences plausibly account for it, and they pull in the same direction.

The first is the mechanism itself. Publishing the candidate bitmap costs a system call, and the scheduler publishes at the end of every loop iteration. Case 1 at heavy load is 3,000 cheap connections per second, so the event loop turns over constantly and each turn pays that cost, while the actual work per connection is only a millisecond. The mechanism is paying full price for information the workload is too uniform to need, since with cheap, uniform requests no worker stays busy long enough for worker-awareness to be worth anything. This is consistent with what the Hermes authors themselves report, where the system calls that update the eBPF map are the single largest component of their overhead budget under heavy load, larger than the scheduler and far larger than the in-kernel dispatcher (Pan et al., 2025).

The magnitude, however, is not comparable with theirs, and this is the second difference. Pan et al. measure that system-call component at under 1% of CPU, whereas the penalty here is a ninefold latency increase. The gap is explained by what the workers are doing: in production they perform real Layer 7 processing, against which a fixed per-iteration overhead is amortised, whereas here they sleep (Section 7.4). A cost that is negligible beside real work is dominant beside no work. The direction of this finding is therefore trustworthy and the mechanism is verified, but its size should be read as an upper bound on the real penalty rather than an estimate of it.

Both differences are compounded by the single-port configuration described in Section 8.1, which removes the O(#ports) cost that, in the paper's own account, is the other main reason epoll exclusive performs badly in this case. The honest summary is that this project has demonstrated a genuine cost of the design that the simulation could not surface, since there the publication step was one memory instruction, but has not reproduced the conditions under which the paper found that cost to be worth paying.

Case 2 (high CPS, high cost, with an injected stall). Under sustained overload, worker-awareness pays. Micro-Hermes's mean latency is 534.8 ms against reuseport's 586.5 ms, and its 99th percentile is the best of the three at 1,441 ms, against 1,651 ms for epoll exclusive and 1,654 ms for reuseport. It also sustains more completed work than reuseport (75.2 against 70.4 connections/s). Epoll exclusive posts the lowest mean (419.3 ms) but the worst tail, and the split between those two numbers is the whole point: the mean says the average connection did well, the 99th percentile says the unlucky ones did badly, and it is the unlucky ones that concentration produces. That Micro-Hermes wins the tail here, on a case that contains a genuine stalled worker, is the result most directly supporting the architecture's purpose.

The advantage over reuseport grows with load, as the paper claims it should. At medium load Micro-Hermes's mean is 195.9 ms against reuseport's 343.5 ms, a ratio of 1.75x; at light load the two are level (87.1 against 88.4 ms), because at 45% utilisation there is nothing yet to route around.

Case 3 (low CPS, long-lived connections). Latency is uniform across policies at every level, as expected when the per-connection cost is trivial. Balance is the real measurement, and it reproduces the paper's headline production result: epoll exclusive ends with a standard deviation of 103.5 connections between workers against 5.4 for Micro-Hermes and 6.0 for reuseport, and on the steady-state measure the gap is starker still (59.7 against 4.4 and 4.6). The ordering matches what Alibaba report from production, where the corresponding figures are 3,200, 50 and 20. As in the simulation, Micro-Hermes matches rather than beats reuseport, for the reason given in Section 8.7.

Case 4 (low CPS, high cost). With expensive requests, Micro-Hermes again beats reuseport across the board and the margin widens with load: at medium load its mean is 153.9 ms against 212.2 ms and its 99th percentile 494.2 ms against 691.2 ms; at heavy load, 242.3 against 307.4 ms and 820.6 against 1,290.7 ms, a tail ratio of 1.57x. Throughput follows: at heavy load Micro-Hermes completes 43.7 connections/s against reuseport's 40.4, because connections sent to already-busy workers wait rather than finish. Epoll exclusive again posts the lowest mean and tail of the three.

Case 5 (synchronised burst). This is where epoll exclusive's low latency elsewhere is paid for. After the same accumulation of long-lived connections, every open connection fires one follow-up request simultaneously. Micro-Hermes completes the burst with a mean of 67.5 ms and a 99th percentile of 224.3 ms, and reuseport is equivalent at 68.0 and 232.9 ms. Epoll exclusive takes 248.3 ms mean and 769.8 ms at the tail, roughly 3.4 times worse. The direct explanation is in the same data: averaged over trials, the burst is served by 4.0 workers under Micro-Hermes and reuseport but only 2.0 under epoll exclusive. Half the machine sits idle while the other half works through a backlog it alone accumulated, and no dispatch decision can fix this after the fact, because a request on an established connection can only be handled by the worker that already owns it.

Taken together, the working system supports the same overall claim as the simulation, with one important addition. Micro-Hermes is never the worst policy when it matters: it holds the best tail latency under overload, balances connections an order of magnitude better than epoll exclusive, and avoids the burst liability entirely. But it is now clearly the worst choice in one specific regime, high volumes of cheap uniform work, and for a reason that is intrinsic to the design rather than incidental to this implementation.

8.4 Worker Hang Prevention Results

Case 2 injects a 400 ms stall into one worker mid-run, reproducing the kind of hang that motivated the original Hermes paper. This is the mechanism the whole architecture exists for, so it is worth examining in both versions.

In the simulation, the recorded scheduler decisions show it working as intended: in every trial the stalled worker is removed from the candidate bitmap within the 200 ms hang threshold of the stall starting (56-192 ms across trials), and readmitted within milliseconds of re-entering its loop.

The exclusion happens in two steps. Usually the worker drops out of candidacy almost immediately, because it stalls while still holding a batch of unprocessed events, and the load filters exclude it on that basis alone. As a backstop, the time filter guarantees exclusion by 200 ms into the stall regardless, once the worker's loop-entry timestamp has gone stale. The worker is readmitted once the stall ends and it re-enters its loop, re-stamping the WST.

The eBPF version shows the same mechanism operating, and Figure 12 traces one episode end to end. The behaviour is messier than the simulation's, in an instructive way. The worker is excluded from candidacy across the whole stall window, as designed. But it is then excluded again, for far longer than the stall itself, while it works through the backlog that accumulated while it was frozen: its pending-event count jumps to nearly thirty immediately after recovery and takes well over a second to drain, and for most of that period the load filters keep it out. Recovery from a hang, in other words, is not instantaneous even once the hang ends, because the worker is genuinely still overloaded. The scheduler treats it accordingly, which is the correct behaviour, and it is behaviour the simulation's cleaner picture understated.

[FIGURE 12 HERE — analysis/figures/fig5_hang_detection.pdf

Caption: eBPF version: hang detection during a Case 2 run with a 400 ms stall injected into worker 0 (shaded). Top: worker 0's pending-event count, which flatlines during the stall and then spikes as the backlog arrives. Bottom: worker 0's presence in the candidate bitmap — excluded across the stall, briefly readmitted when it resumes, then excluded again while it drains the backlog the stall created.]

In both versions the exclusion is carried out by the other workers' schedulers, since the stalled worker cannot run its own. This confirms the value of embedding the scheduler in every worker rather than running it as a separate process: the component that notices a failure is never the component that failed. Over the same window, the reuseport baseline keeps assigning roughly its usual share of new connections to the stalled worker, which is exactly the failure mode that motivated Hermes's timestamp-based detection.

8.5 Scheduler Filter Behaviour

The per-stage survivor counts recorded at every scheduling pass show the cascading filter behaving as the paper describes, in both versions. In the simulation, under light load (Cases 1 and 3) the filters barely prune anything: on average 3.7 to 3.8 of the four workers survive all three stages.

The eBPF version shows the same pattern more sharply (Figure 13). In the light cases the filters prune nothing at all, leaving all 4.0 workers as candidates, which is the desired behaviour: when no worker is struggling, the scheduler should not be narrowing the field. Under the overloaded Case 2 it prunes hardest, with the average candidate set falling from 2.8 workers after the liveness filter to 2.3 after the connection-count filter and 2.2 after the pending-event filter. Case 4, the other expensive case, sits between the two extremes at 3.7, 3.2 and 3.2.

[FIGURE 13 HERE — analysis/figures/fig6_cascade_stages.pdf

Caption: eBPF version: mean number of workers surviving each stage of the scheduler's cascading filter, per case (Hermes runs; error bars: ±1 sd across trials). The light-load cases prune nothing; the overloaded Case 2 prunes hardest; and only the two expensive cases see the liveness filter remove anyone.]

In the simulation the corresponding figures are 3.5 survivors after the liveness filter in Case 2 and 2.6 after the third, following the same ordering. In both versions, Case 2 is where the liveness filter itself prunes hardest, and in neither is this only the injected stall: a worker that spends several hundred milliseconds inside a single loop iteration processing a batch of slow events looks, from the status table's point of view, exactly like a hung worker, and gets treated as one. This is arguably correct, since either way it cannot accept new work promptly.

Overall, both versions match the paper's observation that the fraction of workers passing the coarse filter shrinks as load increases, while theta's margin stops the set collapsing to a single candidate — a collapse that would forfeit the kernel's fine-grained selection and trigger the fallback path instead. That the pruning is sharper in the eBPF version is consistent with its workers facing real costs the simulation did not impose on them.

8.6 Comparing the Two Versions

Building the same architecture twice, once against a simulated operating system and once against a real one, makes it possible to ask a question most replications cannot: which of the simulation's conclusions were about the architecture, and which were about the simulation? Because both versions ran the identical experiment, the differences between them are attributable to what changed, namely the machinery underneath. Three findings follow, and they are of different kinds.

What the simulation got right. The central claims survive the move to a real kernel unchanged. Epoll exclusive concentrates connections catastrophically, and the real mechanism does it just as badly as the model predicted: in Case 3 the simulation measured a standard deviation of 104 connections against roughly 15 for the alternatives, and the real mechanism gives 103.5 against 5.4 and 6.0. Micro-Hermes beats reuseport whenever workers are busy or stalled, and the margin widens with load, in both versions. The burst liability in Case 5 is real, and the eBPF version quantifies its mechanism directly: the burst is served by only 2.0 workers under epoll exclusive against 4.0 under the other two. Hang detection works, carried out by the surviving workers' schedulers. For a system reconstructed from a paper with no access to its source, this is a substantial vindication of the modelling.

What the simulation could only partly explain. Section 8.7 argued that the simulated epoll-exclusive baseline was flattered by an unrealistic advantage: when every worker was busy it consulted per-worker backlogs that the real mechanism does not possess, and concluded that the latency comparison was therefore "conservative towards Micro-Hermes", with the real gap likely larger in Micro-Hermes's favour. Removing that shortcut was one of the reasons for building the second version, and the outcome is informative but not decisive.

The shortcut is certainly gone. The baseline is now the kernel's own wait-queue behaviour, with genuinely no per-worker backlogs to inspect, because that design does not have any. Yet epoll exclusive still posts the lowest mean latency in Cases 2 and 4. So the modelling shortcut was not what produced its advantage, and that specific piece of the simulation's reasoning does not survive.

It does not follow that the prediction itself was wrong, and this is where the project must stop short of a conclusion. The paper attributes epoll exclusive's poor showing to two costs, and this evaluation has only removed one of them from consideration. The other is the O(#ports) dispatch overhead described in Section 8.1, which a single-port benchmark cannot exhibit at all. Real epoll exclusive as measured here is real in its wakeup behaviour but is running in a configuration that eliminates the cost the paper identifies as central. The correct statement is therefore that this project has refuted its own earlier explanation for the gap without establishing what the gap would be under production conditions, and that closing the question requires a multi-port benchmark rather than a better model.

What can be said with confidence is narrower and does not depend on port count. Waking whichever worker is idle is a very good heuristic for mean latency and needs no information to apply, but it buys that mean by loading a few workers heavily. That is precisely what produces the concentration in Case 3, the burst penalty in Case 5, and the worst tail latency of the three policies under Case 2's overload. Hermes's value lies in protection against the tail and against accumulated imbalance, which is what the architecture was designed for and what the production incidents motivating it actually involved. On mean latency against epoll exclusive, this project's evidence is inconclusive.

What only the real system could show. The syscall cost of publishing worker status does not exist in the simulation, where writing the candidate bitmap is a single memory instruction. In the eBPF version it is a system call paid at the end of every loop iteration, and under Case 1 at heavy load, where connections are numerous and individually cheap, it inverts the ranking completely: 26.5 ms mean for Micro-Hermes against 3.0 ms for epoll exclusive. The simulation reported Micro-Hermes as the best policy in exactly this cell. This is the sharpest illustration of the general point that a simulation cannot price what it does not implement, and it is a real limit on where the architecture should be deployed rather than a defect in this implementation. It also corroborates, from the outside, the Hermes authors' own overhead accounting, in which map-update system calls are the largest single component of the mechanism's cost under heavy load (Pan et al., 2025).

The overall picture is that the architecture's value is real but conditional. It is worth paying for when per-connection work is expensive or variable, when workers can stall, or when connections are long-lived enough for imbalance to accumulate. Under the conditions measured here it is not worth paying for when work is cheap and uniform, because the cost of continuously publishing status then exceeds the value of the information. That last boundary should be read with the two qualifications given in Section 8.3, namely that the penalty is inflated by workers who sleep rather than work, and that a single-port benchmark removes a cost the paper attributes to the alternative. The paper's own production data suggests the favourable condition is usually the one that holds, since the two regimes that dominate Alibaba's traffic are long-lived connections and expensive processing (Section 2.8), which are exactly the two where Micro-Hermes performs well here.

8.7 Discussion

This section records what the evaluation does not establish, and revisits the reasoning the simulation used before the real system was available to check it against.

The simulation's epoll-exclusive baseline was better informed than the mechanism it modelled. Real SO_REUSEPORT, and therefore both reuseport and Micro-Hermes which build on it, gives every worker its own listening socket with its own queue, and the simulation reproduced this faithfully. Real epoll exclusive is different: all workers share one socket and one queue, so the kernel's only decision is which idle worker to wake, and there are no per-worker backlogs for it to inspect. The simulated baseline nonetheless reused the per-worker queues the other two policies genuinely require, and when every worker was busy it handed each connection to whichever queue was shortest, a load-aware decision the real mechanism cannot make. Two further factors pointed the same way: Micro-Hermes rebuilds its candidate list only once per loop iteration and so acts on slightly stale information, whereas the simulated baseline re-examined every worker's queue on each dispatch; and the simulation omitted real kernel-side costs of the exclusive mechanism entirely, including that waking a worker means walking a queue whose length grows with the number of ports being listened on, where this project uses one port and a production load balancer uses tens of thousands.

The inference drawn from this at the time, that the comparison was conservative towards Micro-Hermes, was tested directly by building the second version, and the specific explanation offered did not hold: the gap did not close when the modelling shortcut was removed (Section 8.6). This is recorded as a corrected explanation rather than quietly dropped, because the reasoning was reasonable on the evidence then available and its failure is itself a result. It is also not the end of the question. The third factor listed above, the port count, remains untested in both versions, since both listen on a single port and the cost in question only appears at many. The simulation's conclusion was therefore reached for the wrong reason but has not been shown to be wrong, and that distinction is the clearest argument this dissertation can make for carrying a replication through to a working system: the second version was what revealed the error, and it is also what makes the remaining gap precisely stateable rather than merely suspected.

A second finding from the simulation does survive scrutiny. Micro-Hermes only matches, rather than beats, reuseport's balance in Case 3, whereas the paper's production data has Hermes ahead (standard deviation 20 against 50). The explanation given was that real connections vary widely in duration, which degrades a stateless hash over time, whereas Case 3's synthetic connections are identical and never close, so the hash stays near-optimal by construction and offers no weakness to exploit. The eBPF version reproduces the same parity (5.4 against 6.0), which is consistent with that explanation, since the workload remained synthetic in both. Testing this properly would require a workload with realistically varied connection lifetimes, which neither version has.

The most significant remaining limitation applies to both versions equally, and is the one identified in Section 7.4: processing is simulated in both. A worker sleeps for its connection's assigned cost rather than performing real Layer 7 work. This means the paper's CPU-overhead profile (0.674% to 2.436% across counters, scheduler, system calls and the in-kernel dispatcher) still cannot be checked in full, because a profile of this system would show a processor that is idle rather than working. The dispatch machinery in the eBPF version is real, and Section 8.6 shows one component of that overhead, the system-call cost, appearing plainly in the results; but the ratio of overhead to useful work, which is what the paper's percentages express, cannot be measured against work that does not exist. Closing this would require replacing the sleep with genuine Layer 7 processing.

The evaluation scale is also deliberately small: four workers on one machine, three trials per point, and synthetic traffic with a fixed cost distribution per case. The results establish orderings and trends rather than production performance figures. Finally, two of Hermes's production subsystems remain out of scope: the proactive termination of connections already pinned to a hung worker, and the cluster-level detection and scaling that handle node-wide overload.

Within these limits, the evaluation supports Hermes's central claims. The three-stage feedback loop balances connections far more evenly than either standard Linux mechanism, detects and routes around a hung worker using only three atomically updated numbers, falls back safely when its candidate set collapses, and delivers the best tail latency of the three policies under sustained overload. It does so at a cost that is negligible when per-connection work is expensive and prohibitive when it is not, a boundary this project can state precisely only because it built the mechanism twice.

9. Conclusions

9.1 Summary of Contributions

This dissertation set out to reimplement, in the open, a closed-source production system known only from its published description, and to test whether that description is sufficient to reproduce the system's claimed behaviour. Its contributions are:

First, the implementation itself (objectives O1, O2 and O5): to the author's knowledge the first open-source implementation of the Hermes architecture — the shared-memory Worker Status Table, the per-worker cascading-filter scheduler, and the hash-among-candidates dispatcher — delivered both as a userspace simulation and as a working system in which the dispatch decision is made by a real program running inside the Linux kernel, each worker owns a real socket, and the two baselines are the operating system's own mechanisms rather than models of them. The paper's lock-free concurrency design is preserved exactly in both.

Second, a two-stage methodology that lets the architecture be checked against itself (supporting O1 and O3). Because the simulation and the working system implement the same design and run the identical experiment, the differences between their results are attributable to what changed underneath. This is what makes it possible to separate conclusions about the architecture from conclusions about the modelling, and Section 8.6 does exactly that, including identifying one conclusion the simulation reached that the real system refutes.

Third, an independent reproduction of the paper's qualitative claims (O3). Across the four published traffic regimes, each swept over three offered-load levels, both versions reproduce the paper's central results: Hermes beats the stateless hash whenever workers are busy or stalled, with a margin that widens as load rises and extends to throughput once offered load nears capacity; it balances long-lived connections an order of magnitude more evenly than exclusive wakeup; it detects an injected worker stall through the other workers' schedulers; and under sustained overload it delivers the best tail latency of the three policies tested.

Fourth, an extension of the paper's evaluation (O4): the synchronised-burst scenario converts the paper's argument for the connection-count metric from a rationale into a measurement. In the working system the burst is served by 4.0 workers under Hermes and only 2.0 under epoll exclusive, which shows directly that exclusive's low average latency elsewhere is bought with a standing liability that falls due the moment its hoarded connections become active.

Fifth, a cost the original paper reports only as a percentage and the simulation could not have produced at all: the mechanism's overhead made visible in end-to-end latency. Because publishing worker status to the kernel costs a system call on every loop iteration, and the loop turns over as fast as connections arrive, that cost can dominate under high volumes of cheap work. The direction of this result is solid and its mechanism is verified in the code, and it corroborates from the outside the paper's own finding that map-update system calls are the largest component of its overhead budget under heavy load. Its magnitude is not comparable with production, for two reasons given in Section 8.3: the workers here sleep rather than work, so the overhead is amortised against nothing, and the single-port configuration removes the corresponding cost from the alternative. The contribution is therefore the demonstration that this cost is real and measurable outside a percentage table, not a production estimate of it.

9.2 Limitations

The central limitation is that processing is simulated in both versions: a worker sleeps for its connection's assigned cost rather than performing real Layer 7 work such as a TLS handshake or a compression pass. The dispatch machinery in the eBPF version is entirely real, and its cost shows up plainly in the results, but the paper's CPU-overhead percentages express a ratio of overhead to useful work, and this system has no useful work to measure against. Closing this would mean replacing the sleep with genuine Layer 7 processing, which is a substantial piece of work in its own right and would change what the project is.

Three further limitations follow from the evaluation's scale and workload. It runs four workers on one machine with three trials per point, so the results establish orderings and trends rather than production performance figures. Its connections are synthetic and uniform within each case, which is why Micro-Hermes matches rather than beats reuseport on balance in Case 3: a stateless hash only degrades when connection lifetimes vary widely, and here they do not (Section 8.7). And both versions listen on a single port where the system being replicated uses roughly ten thousand, which removes a cost that falls on the epoll-exclusive baseline alone and therefore leaves this project unable to settle the mean-latency comparison against it (Sections 8.1 and 8.6). Of the three, the port count is the one that most directly limits a conclusion this dissertation would otherwise be able to draw.

Finally, two of Hermes's production subsystems are out of scope entirely: the proactive termination of connections already pinned to a hung worker, and the cluster-level anomaly detection and progressive scaling that handle node-wide overload. Both address failures that a single node's dispatch decisions cannot fix.

9.3 Remaining Work

The most valuable next step follows directly from the limitation above: replace the simulated processing cost with real Layer 7 work, so that a worker performing a TLS handshake or compressing a response is actually consuming a processor rather than sleeping. This is the single change that would unlock the paper's overhead comparison, since the percentages it publishes are ratios of overhead to useful work and this system currently has no useful work in the denominator. It would also make the Case 1 finding sharper: the system-call cost that dominates there is currently being weighed against an idle processor, and a fair accounting needs real work on the other side of the scale.

A second direction is scale. The paper's deployments run tens of workers across tens of thousands of ports, whereas this project runs four workers on one port. Both the candidate bitmap and the filters were designed for the larger case but have only been exercised at the smaller one, and the multi-tenancy effects described in Section 2.1, where the cost of waking workers grows with the number of ports being listened on, cannot appear at all with a single port. That effect is one of the paper's stated reasons for preferring its approach over epoll exclusive, and it is precisely the kind of cost that would shift the balance of this dissertation's Cases 2 and 4, where epoll exclusive currently leads. Of everything listed in this section, this is the one that would resolve an open question rather than add a new result: Section 8.6 can say what epoll exclusive's advantage is not caused by, but cannot say whether it would survive a realistic port count, and only a multi-port benchmark can answer that.

A third follows from the workload rather than the system. Case 3's connections are identical and never close, which is why a stateless hash performs so well there and why Micro-Hermes only matches it. A workload with realistically varied connection lifetimes would test whether the paper's production advantage over reuseport reproduces outside production.

Two of Hermes's production subsystems also remain unimplemented and would complete the reliability story: proactively terminating connections already pinned to a hung worker so their clients reconnect through the healthy dispatch path, and the cluster-level detection and scaling that respond when every worker on a node is saturated. Finally, the working system now makes it cheap to experiment with the policy itself, which is something the closed original cannot offer: alternative filter orderings, a dynamically tuned θ, or entirely different candidate-selection rules can all be evaluated against the same harness and the same five scenarios.

The broader conclusion is encouraging for replication research. A carefully written systems paper, with no released code, contained enough architectural detail to rebuild both its behaviour and its costs, and where this reimplementation diverged from expectation the divergences were explicable and, in one instance, corrected a conclusion the author had drawn from the simulation alone. That correction is the strongest practical argument this project can offer for carrying a replication through to a working system rather than stopping at a model of one. Status-aware dispatch of the Hermes kind is not tied to hyperscale infrastructure: it is a small, portable idea, three shared counters, two filters and a bitmap, that this project now makes available to anyone, along with a measured account of when it is worth using and when it is not.

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

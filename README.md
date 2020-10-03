# SprayCC - a distributed build wrapper
SprayCC is niche tool focused on those of us building medium- to large-scale C++ applications on
clusters of hundreds or thousands of CPUs managed by tools like [LSF](https://www.ibm.com/support/knowledgecenter/en/SSETD4/product_welcome_platform_lsf.html), 
[PBS Pro](https://www.pbspro.org/), or [OpenLava](https://en.wikipedia.org/wiki/OpenLava).

What is "medium or large" to me? Typically 250 kLOC up into the million LOC and hudreds or thousands 
of compilation units. Or, said another way, compile times in the hours if only a single machine were used. 
C and C++ are what I'm familiar with, but other langauges and even unit test runs are potential fits.

SprayCC is * *very* * much a work in progress!

# Prior Art
Tools such as [DistCC](https://github.com/distcc/distcc) and [Icecream](https://github.com/icecc/icecream) provide 
similar capability and inspired some of the development of SprayCC. However, both of these tools rely on running 
a daemon on the build machines to provide compilation services. This is appropriate for dedicated build machines 
or a group of development machines sharing compute resources. However, jobs on the sorts of clusters targeted by SprayCC 
are controlled by the cluster manager, and range from single-unit compilation or unit test tasks, to interactive
sessions, to multi-hour/multi-CPU batch jobs. Running an independent daemon is not possible and trying to manage the daemon
lifetime via the cluster manager is clumsey.

# Project goals
The loose goals are:
1. Provide an opportunity for me to learn [Rust](https://www.rust-lang.org/) by doing something "real"
2. Reduce compilation times for those in this niche by reducing scheduling latency and avoiding NFS cache delays

# Problem description
TODO: scheduling latency, NFS cache headaches, bursty interleaving of jobs

# Execution structure
SprayCC consists of a single executable run in one of three modes:
* **Runner**: The runner is what is invoked from 'make' or another build tool as a wrapper around a compile 
line. The runner process simply submits the task to the server and waits for the output (stdout, stderr, or 
produced files) to be returned. Once the task is complete the runner exits with the appropriate exit code.
* **Server**: Exactly one server is started per user, per workspace. The server queues requests from the runner 
processes, starts executors on the cluster, distributes the tasks to the running executors, forwards results 
from the executors to the appropriate runners, and then shuts everything down at the end.
* **Executor**: The executor processes run on the cluster and accept task, run the task locally, and send back 
both stdout/stderr and the contents of any files produced by running the task. For example, a typical compile 
job will send back either a stream of error messages or the generated .o file.

The server submits one executor for every CPU to be used during the build process. The executor holds onto the 
CPU slot for the entire duration of the build. This eliminates the scheduling overhead and latency of submitting 
hundreds or thousands of small jobs to the cluster manager, while also eliminating NFS cache overhead since the 
generated files are returned to the originating host.

# Why Rust?
*Reason number one:* I want to learn Rust and the best way to learn a language is to do something "real". 
This is a nice learning project because it is small, and requires reasonably performant I/O and managing 
state across distributed processes.

*Why not use a higher level language?* There will be one runner process on the submitting machine for 
every parallel job running. That could add up to thousands on a single, shared, interactive machine if multiple people 
are doing heavily distributed jobs. Given that, the processes have to be extremely lightweight, no overhead 
of a runtime such as the JVM or Python.

Similarly, compile jobs are often short, so startup time is important thus, again, a language runtime is likely prohibitive.

*So why not C or C++?* Because doing stuff like this in C and C++ is hugely tedious. And, I've already done it, I don't
want to do it again.

# Schedule and contributions
This is an as-evening-time-permits project -- there is no schedule.

Contributions, questions and suggestions welcome.

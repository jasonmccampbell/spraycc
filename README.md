# SprayCC - a distributed build wrapper
SprayCC is niche tool focused on those of us building medium- to large-scale C++ applications on
clusters of hundreds or thousands of CPUs managed by tools like [LSF](https://www.ibm.com/support/knowledgecenter/en/SSETD4/product_welcome_platform_lsf.html), 
[PBS Pro](https://www.pbspro.org/), or [OpenLava](https://en.wikipedia.org/wiki/OpenLava).

What is "medium or large" to me? Typically 250 kLOC up into the millions of LOC and hundreds or thousands 
of compilation units. Or, said another way, compile times in the hours if only a single machine were used. 
C and C++ are what I'm familiar with, but some unit test suites work well and most other flows driven by command-line-invoked steps are possible.

SprayCC is functional and useful, though could use some polish. See Usage below.

# Prior Art
Tools such as [DistCC](https://github.com/distcc/distcc) and [Icecream](https://github.com/icecc/icecream) provide 
similar capability and inspired some of the development of SprayCC. However, both of these tools rely on running 
a daemon on the build machines to provide compilation services. This model is appropriate for dedicated build machines 
or a group of development machines sharing compute resources but doesn't work well on the sorts of clusters targeted by SprayCC, 
which are organized as a set of queues handling a range of tasks from single-unit compilation or unit tests, to interactive
sessions, to multi-hour/multi-CPU batch jobs. Running an independent daemon is not possible and trying to manage the daemon
lifetime via the cluster manager is clumsy.

# Project goals
The loose goals are:
1. Reduce compilation times for those in this niche by eliminating scheduling latency and using acquired CPU resources for
multiple tasks, rather than separately jobs for every compilation task.
1. Avoid NFS cache delay headaches which can introduce build errors or build delays waiting for the cache on one machine
to reflect results written by another.
1. Provide an opportunity for me to learn [Rust](https://www.rust-lang.org/) by doing something "real"

# Usage
To use SprayCC to build an application, first setup the .spraycc configuration file in your home directory. The file
spraycc.toml in the source tree provides a template. The most important value to customize is the 'start_cmd' value
which specifies how to start a SprayCC take in cluster.

Once the configuration file is set, the server can be started with:

    spraycc server

It will report the IP address and port the server is listening on, which configuration file was read, and the start command
which will be used. This is a good sanity check that all is in order, and the server will exit after some period
(idle_shutdown_after) if unused. The server needs to be started in the root tree of the project to be built, or above.

To submit tasks using SprayCC, each compile step needs to be prefixed with "spraycc run". For example:

    spraycc run g++ -c hello.cpp -o hello.o

SprayCC will search for the file _.spraycc_server_ in the current directory, or, if not found, each parent directory up to
the root until found. This instructs it on how to find the server. Once found, the compilation command is submitted to the
server. The server starts executors as needed and distributes the submitted jobs across all available executors.

How the 'spraycc run' prefix is added depends on the structure of your Make system. In many cases, _bsub_, _gridsub_, or
other submit command is already inserted and it is just a matter of using 'spraycc run' instead.

Now it is time to do a trial build. For the first run, it can be helpful to start the server in a separate terminal
from where the build will be performed. In one terminal, start the server:

    > spraycc server
    Spraycc: Server: 10.100.200.300:45678
    SprayCC: Read config file from /home/jason/.spraycc
    SprayCC: Start command: bsub -q linux -W 1.0 /tools/bin/spraycc exec --callme=10.100.200.300:45678 --code=42

In a different terminal, start the build. In some environments it is common to dump all compile jobs into the queue at
once, and in others it is preferable to limit the number of CPUs used at with "make -j100" (limit of 100 CPUs) or similar.
SprayCC is designed to work best with as large a queue of jobs as possible because it scaled well and eventually it will
prioritize the queue to run longer jobs first.

As the job runs, the server will periodically update its status:

    SprayCC: 19 / 60 finished, 4 running, 4 executors, 243.64KiB / sec

The server will start with 'initial_count' executors, and add 'keep_pending' additional jobs until 'max_count' is
reached. This avoids saturating a busy queue with too many jobs, but still allows the total number of CPUs to be
high if the cluster usage is low. The configuration setting 'release_delay' specifies how long to wait after the most
recent task submission to start releasing executors idle executors and thus CPU resources back to the queue.

# Execution structure
SprayCC consists of a single executable run in one of three modes:
* **Runner**: The runner is what is invoked from 'make' or another build tool as a wrapper around a compile 
line. The runner process simply submits the task to the server and waits for the output (stdout, stderr, or 
produced files) to be returned. Once the task is complete the runner exits with the appropriate exit code.
* **Server**: Exactly one server is started per user, per workspace. The server queues requests from the runner 
processes, starts executors on the cluster, distributes the tasks to the running executors, forwards results 
from the executors to the appropriate runners, and then shuts everything down at the end.
* **Executor**: The executor processes run on the cluster and accept tasks, run the task locally, and send back 
both stdout/stderr and the contents of any files produced by running the task. For example, a typical compile 
job will send back either a stream of error messages or the generated .o file. The .o file is written to local storage
on the cluster machine and transmitted back to the server.

The server submits one executor for every CPU to be used during the build process. The executor holds onto the 
CPU slot for the entire duration of the build, scaling down once the task queue depletes. This eliminates the 
scheduling overhead and latency of submitting hundreds or thousands of small jobs to the cluster manager, while
also eliminating NFS cache overhead since the generated files are returned to the originating host.

# Why Rust?
*Reason number one:* I want to learn Rust and the best way to learn a language is to do something "real". 
This is a nice learning project because it is small, and requires reasonably performant I/O and managing 
state across distributed processes.

*Why not use a higher level language?* There will be one runner process on the submitting machine for 
every parallel job running. That could add up to thousands on a single, shared, interactive machine if multiple people 
are running heavily distributed jobs. Given that, the processes have to be extremely lightweight, no overhead 
of a runtime such as the JVM or Python.

Similarly, compile jobs are often short, so startup time is important thus, again, the startup time of many language
runtimes is prohibitive (Python and Java, I'm looking at you).

*So why not C or C++?* Because doing stuff like this in C and C++ is hugely tedious, even if using Boost and other
libraries, if only because setting everything up is pain. 

And there is a second reason: lack of high-level async support. I originally wrote this project in Rust using the 
Metal I/O ([MIO](https://crates.io/crates/mio)) crate for low-level asynchronous I/O, which is at roughly the same level
of abstraction as Boost's ASIO library. This meant writing the state machines for all of the communication by hand,
which quickly became unproductive when I would get only an hour to work on it once, maybe twice, in a week.

After a gap of a year where I was too busy to work on this project, Rust's async / await features stabilized,
and the excellent [Tokio crate](https://crates.io/crates/tokio) matured. I don't know if when I originally looked
at it process support wasn't there yet or whether I didn't know enough to understand it, but now it is very solid
and I found [Tokio's select!](https://tokio.rs/tokio/tutorial/select) macro. I deleted 80% of the original code and
in a single (late!) night got the basic system up and running again.

Those of you familiar with Go, Node JS, Python and similar languages are probably saying something along the lines
of "no, duh". However, I had not expected there to be such as massive productivity difference from C/C++ (I've been
using C++ daily from just about the beginning). One side goal of this project was originally as a demonstration of
Rust's advantages as a C/C++ replacement but that's somewhat shot since there isn't even comparable functionality.
The development style is now completely different.

# Schedule and contributions
This is an as-evening-time-permits project -- there is no schedule.

Contributions, questions, criticisms and suggestions are always welcome.

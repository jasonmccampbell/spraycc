# SprayCC - a distributed build wrapper
SprayCC is niche tool focused on those of us building large-scale C++ applications (hundreds or thousands of kLOC) on
large CPU clusters managed by tools like [LSF](https://www.ibm.com/support/knowledgecenter/en/SSETD4/product_welcome_platform_lsf.html), 
[PBS Pro](https://www.pbspro.org/), or [OpenLava](https://en.wikipedia.org/wiki/OpenLava). Applications such as these
can takes multiple CPU hours to build, so distributing builds across a hundred or more CPUs can significant
improve development productivity. SprayCC is designed to do this and avoid cache coherency issues with distributed
file systems as well.

SprayCC is functional and useful, though still being developed.

# Project goals
The loose goals are:
1. Reduce compilation times for those in this niche by eliminating scheduling latency through using acquired CPU resources for
multiple tasks, rather than using separate jobs for every compilation task.
1. Avoid NFS cache (in)consistency headaches which can introduce build errors or failures.
3. Provide an opportunity for me to learn [Rust](https://www.rust-lang.org/) by doing something "real".

# Quick start
You can get started with just three (and a half) steps.

**One**: Copy the file *spraycc.toml* from the source directory to *.spraycc* in your home directory. There are
multiple customization options, but 'start_cmd' is the only one which needs to be configured initially. This
is the command you want it to run to submit a task to the cluster.

**One-point-five**: Run 'spraycc init' to generate the user-key and double-check that SprayCC can get enough
file descriptors to run well:

    > spraycc init
    SprayCC: Successfully increased file descriptor limit to 4096
    SprayCC: Generated user-private key in /home/jason/.spraycc.private

This step only needs to be run once. If skipped, the first time the server starts it will create the
user-private key and nearly all of the time it will work fine (see 'Race condition creating user-private
key' below).

**Two**: Start the server in the root of your source tree:

    > spraycc server
    Spraycc: Server: 10.100.200.300:45678
    SprayCC: Read config file from /home/jason/.spraycc
    SprayCC: Start command: bsub -q linux -W 1.0 /tools/bin/spraycc exec --callme=10.100.200.300:45678 --code=42

It reports the port it is listening on and the command which will be used to submit jobs to your cluster. If you
see this output, you are ready to compile.

**Three**: Run a build task by prefixing the compilation line with 'spraycc run':

    spraycc run g++ -c hello.cpp -o hello.o
    
The command line plus environment (directory, environment variables, etc) are submitted to the server, which starts
a process in the cluster and executes this step.

To run a full build, modify the build system to prefix each compilation step with 'spraycc run'. SprayCC will
distribute the compile steps and keep the link steps locally (it understands GCC/G++ and Clang command lines). When
a full build is run, use a lot more parallelism (5-10x) than the number of CPUs available so that the server has a large
queue to work with. For example:

    make -j1000 RELEASE=1
    
The 'spraycc run' process is lightweight so it should not bog the local system down. Dependency generation and
compilation steps are run in the cluster; linking and archive ('ar') tasks are run locally to ensure these
steps see the correct versions of each compiled file.

The first time a build is run the tasks will be processed in the order in which they are submited. The server
records the elapsed time for each task and on subsequent runs will priorize the queue accordingly to reduce
the overall build time.

The server process will terminate after being idle for a period of time.

# Execution structure
SprayCC consists of a single executable run in one of three modes:
* **Runner**: The runner is what is invoked from 'make' or another build tool as a wrapper around a compile 
line. The runner process submits the task (compilation step, test case execution or anything else) to the server 
and waits for the output (stdout, stderr, or produced files) to be returned. Once the task is complete the runner
exits with the appropriate exit code.
* **Executor**: The executor processes run in the cluster and accept tasks, run the task locally, and send back 
both stdout/stderr and the contents of any files produced by running the task. For example, a typical compile 
job will send back either a stream of error messages or the generated .o file. The .o file is written to local storage
on the cluster machine and transmitted back to the server.
* **Server**: Exactly one server is started per user, per workspace. The server queues requests from the runner 
processes, starts executors on the cluster, distributes the tasks to the running executors, forwards results 
from the executors to the appropriate runners, and then shuts everything down at the end.

The server submits one executor for every CPU to be used during the build process. The executor holds onto the 
CPU slot for the entire duration of the build, scaling down once the task queue depletes. This eliminates the 
scheduling overhead and latency of submitting hundreds or thousands of small jobs to the cluster manager, while
also eliminating NFS cache overhead since the generated files are returned to the originating host.

At startup the server writes the file  _.spraycc_server_ in the current directory. This is the file that the runner
processes will look for in order to know how to connect to the server. Each time the server starts, it creates
a randon access code which is included in this file. This prevents runner and exector processes (and port scanners
or other software) from accidentally connecting to the wrong server instance.

If you are using symbolic links in your file system, please be aware that the runner processes will look 'up'
the real directory tree of the directory where the process is started, not back through the symbolic links
that may have been traversed to get there. If the server file .spraycc_server is not in a parent of the
directory the runner started it, it will report that the server hasn't been started.

# Server options and multiple queues
The server mode supports a couple of useful options. Specifically:

    --alt     Submit executors using the 'alt_start_cmd' in ~/.spraycc
    --both    Submit executors alternating between the two start commands
    --cpus=N  Set the maximum number of CPUs to use, overriding 'max_count' in ~/.spraycc
    
In clusters with multiple available queues, the --alt and --both options allow you to take advantage of different
pools of CPUs. When --both is used, half the initial count of executors is submitted to each queue. Then as
executors start, new pending jobs are added to that same queue where the new one came from. This has the effect
of loading up the queue that has available resources.

--cpus=N is useful for grabbing more or fewer CPUs.

# History-based scheduling
Most of us compile applications. A lot. And similarly run test suites over-and-over. SprayCC makes use of this to
reduce runtimes by recording the elapsed time for each task that completes successfully. These timings are recorded
in ~/.spraycc.history.

The next time the server starts it reads the recorded history and uses it to priorize the job queue, longest tasks
first. This helps to keep it using the full number of CPUs until right at the end. Dependency-generation tasks
(-M or -MM in Gnu compilers) are bumped up in priority to ensure they run early since they frequently gate the
starting of additional tasks.

In order to keep the history from growning forever, historical enteries are purged after ~6 months.

# Race condition creating user-private key
The user-private key is stored in ~/.spraycc.private and contains a randomly-generated number that is very likely
unique to each user. All of the processes read this file and the runner and executor processor send this key,
as well as the per-session access code, to the server. This helps prevent one user from accidentally (or
intentionally) connecting to another user's server.

Creation of this key is done through the 'spraycc init' command or the first time 'spraycc server' is run.
If the file is created when the server starts, it is very likely that everything will work just fine. However,
there is a race condition such that if an executor process starts in the cluster and that host happens to
have a cached view of your home directory that doesn't have the private key in it, the executor won't see
the newly created key and exit. This is only an issue the very first time SprayCC is used, and I haven't
actually seen this issue come up, but it could happen.

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

*So why not C or C++?* Partly because I wanted to see how Rust compares to C/C++, which I use professionally. But also
because doing stuff like this in C and C++ is hugely tedious, even if using Boost and other libraries, if only because
setting everything up is pain. 

After getting into this project, there is a *huge* additional reason: Rust's async support. I originally wrote this project
in Rust using the Metal I/O ([MIO](https://crates.io/crates/mio)) crate for low-level asynchronous I/O, which is at
roughly the same level of abstraction as [Boost's ASIO library](https://www.boost.org/doc/libs/1_76_0/doc/html/boost_asio.html).
This meant writing the state machines for all of the communication by hand, which quickly became unproductive when I would
get only an hour to work on it once, maybe twice, in a week.

After a gap of a year where I was too busy to work on this project, Rust's async / await features stabilized,
and the excellent [Tokio crate](https://crates.io/crates/tokio) matured. I don't know if, when I originally looked
at it, Tokio's process support wasn't ready yet or whether I didn't know enough to understand it, but now it is very solid
and I found [Tokio's select!](https://tokio.rs/tokio/tutorial/select) macro. I deleted 80% of the original code and
in a single (late!) night got the basic system up and running again.

Those of you familiar with Go, Node JS, Python and similar languages are probably saying something along the lines
of "no, duh". However, I had not expected there to be such as massive productivity difference from C/C++ (I've been
using C++ daily from just about the beginning). One side goal of this project was originally as a demonstration of
Rust's advantages as a C/C++ replacement but that's somewhat shot since there isn't even comparable functionality.
The development style is now completely different.

In short, huge props to the Tokio and async developers and contributors!

# Schedule and contributions
This is an as-evening-time-permits project -- there is no schedule.

Contributions, questions, criticisms and any other feedback are always appreciated.

# SprayCC - a distributed build wrapper
SprayCC is niche tool focused on those of us building large-scale C++ applications (hundreds or thousands of kLOC) on
large CPU clusters managed by tools like [LSF](https://www.ibm.com/support/knowledgecenter/en/SSETD4/product_welcome_platform_lsf.html), 
[PBS Pro](https://www.pbspro.org/), or [OpenLava](https://en.wikipedia.org/wiki/OpenLava). Applications such as these
can takes 10's of thousands of CPU seconds to build, so distributing builds across a hundred or more CPUs can significant
improve development productivity. SprayCC is designed to do this and avoid cache coherency issues with distributed
file systems as well.

SprayCC is functional and useful, though several features and some polish are still planned.

# Project goals
The loose goals are:
1. Reduce compilation times for those in this niche by eliminating scheduling latency and using acquired CPU resources for
multiple tasks, rather than using separate jobs for every compilation task.
1. Avoid NFS cache delay headaches which can introduce build errors or failures.
3. Provide an opportunity for me to learn [Rust](https://www.rust-lang.org/) by doing something "real"

# Quick start
You can get started with just three steps.

**One**: Copy the file *spraycc.toml* from the source directory to *.spraycc* in your home directory. There are
multiple customization options, but 'start_cmd' is the only one which needs to be configured initially. This
is the command you want it to run to submit a task to the cluster.

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
a full build is run, use a lot more parallelism than the number of CPUs available so that the server has a large
queue to work with. For example:

    make -j1000 RELEASE=1
    
The 'spraycc run' process is lightweight so it should not bog the local system down.

The server process will terminate after being idle for a period of time.

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

At startup the server writes the file  _.spraycc_server_ in the current directory. This is the file that the runner
processes will look for in order to know how to connect to the server. Each time the server starts, it creates
a randon access code which is included in this file. This prevents runner and exector processes (and port scanners
or other software) from accidentally connecting to the wrong server instance.

If you are using symbolic links in your file system, please be aware that the runner processes will look 'up'
the real directory tree of the directory where the process is started, not back through the symbolic links
that may have been traversed to get there. If the server file .spraycc_server is not in a parent of the
directory the runner started it, it will report that the server hasn't been started.

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

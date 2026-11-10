----------------------------- MODULE BoatRunner -----------------------------
(***************************************************************************)
(* One sw-boat-runner pool: GitHub's queue and runners, boat's machines,  *)
(* and the reconciler of docs/design.md as src/decide.rs and src/run.rs   *)
(* implement it. Stop and Delete are decided from one snapshot and        *)
(* executed later, because run::execute does not look again; every other  *)
(* tool action reads the current state. The Fix* constants switch on the  *)
(* fixes in spec/README.md. The code at ae2ef64 has all nine.             *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
    Machines,       \* boat machine ids; the ones not in InitStopped start Gone
    InitStopped,    \* kept machines that are Stopped at the start
    Jobs,           \* job keys (id and attempt); a re-run is another key
    NoJob,
    NoMachine,
    Placement,      \* subset of {"slot", "resume", "fork"}
    BoxCap,         \* max_boxes
    MinUp,          \* min_up
    Included,       \* included starts left today
    PaidBudget,     \* paid_start_cents_per_day, in units of one start's price
    MaxFaults,      \* faults per behaviour, shared by every fault kind
    FixCrashSpend,  \* a restart sets spent_today to the cap, not to 0
    FixRegSeen,     \* Register marks the machine as having had a runner
    FixCrashSeen,   \* after a restart an Up ephemeral machine counts as used
    FixForkKey,     \* a job fork's Idempotency-Key changes once its machine is Gone
    FixStaleAct,    \* Stop and Delete first remove the machine's GitHub runner,
                    \* which GitHub refuses while it runs a job
    FixKeptInflight,\* after a restart the tool starts nothing until every fork
                    \* sent before the crash is listed or has failed
    FixListLag,     \* the tool counts the forks it created until the listing
                    \* shows them
    FixLostCreate,  \* after a create with no definite answer the tool starts
                    \* nothing until boat lists the machine
    FixLostSpend    \* a create with no definite answer counts its price as spent

ASSUME
    /\ InitStopped \subseteq Machines
    /\ Placement \subseteq {"slot", "resume", "fork"}
    /\ Placement # {}
    /\ MinUp > 0 => "slot" \in Placement
    /\ BoxCap \in Nat /\ MinUp \in Nat /\ Included \in Nat
    /\ PaidBudget \in Nat /\ MaxFaults \in Nat
    /\ {FixCrashSpend, FixRegSeen, FixCrashSeen, FixForkKey, FixStaleAct,
        FixKeptInflight, FixListLag, FixLostCreate, FixLostSpend} \subseteq BOOLEAN
    /\ NoJob \notin Jobs
    /\ NoMachine \notin Machines

VARIABLES
    mstate,       \* boat: Starting, Up, Stopped, Gone (Stopping is folded into Stopped)
    job,          \* boat label: the job a machine was forked for; NoJob on a kept machine
    keep,         \* boat: a stopped machine's disk is still kept, so a resume is free
    runner,       \* GitHub: the runner on the machine's one slot
    status,       \* GitHub: Pending (not yet arrived), Queued, Running, Done
    jobOn,        \* GitHub: the machine running the job
    seen,         \* tool memory: last_busy >= since on an ephemeral machine
    idle,         \* tool memory: idle_stop_s since last_busy and min_up_s since since
    age,          \* now - since: 0, 1 past register_timeout_s, 2 past fork_ttl_s
    plan,         \* tool: a Stop or Delete decided this tick and not yet executed
    starts,       \* boat: counted starts today
    spent_today,  \* tool memory: Limits.spent_today_cents, in start prices
    keys,         \* boat: job fork Idempotency-Keys already used today
    ran,          \* ghost: a job ran on this ephemeral machine
    lag,          \* boat's listing and tool memory of a created machine; see Vis
    faults

vars == <<mstate, job, keep, runner, status, jobOn, seen, idle, age, plan,
          starts, spent_today, keys, ran, lag, faults>>

-----------------------------------------------------------------------------
(* What decide derives from one snapshot. *)

Count(S) == Cardinality(S)
Has(step) == step \in Placement

(* boat lists a created machine some time after the create returns. Listed: *)
(* in the listing. Known: not yet listed; the tool remembers creating it and *)
(* counts it as a Starting machine. Hidden: not yet listed and not counted.  *)
(* Lost: not yet listed, and the create's answer never reached the tool.    *)
(* Orphan: not yet listed and created before a restart, which lost the      *)
(* tool's memory. The listing catches up before the machine boots; an Up    *)
(* machine missing from it would add only a delay, since the tool registers *)
(* runners on listed machines alone.                                         *)
Vis(m) == lag[m] \in {"Listed", "Known"}
Grows == Has("resume") /\ ~Has("fork")
Live(m) == mstate[m] \in {"Starting", "Up"}
IsEph(m) == job[m] # NoJob
Kept(m) == job[m] = NoJob /\ mstate[m] # "Gone"
PastTtl(m) == IsEph(m) /\ age[m] = 2
Boxes == Count({m \in Machines : Live(m)})
ToolBoxes == Count({m \in Machines : Live(m) /\ Vis(m)})
Queued == {j \in Jobs : status[j] = "Queued"}
UpKept == Count({m \in Machines : Kept(m) /\ mstate[m] = "Up"})
StoppedKept == {m \in Machines : Kept(m) /\ mstate[m] = "Stopped"}

\* Expired is GitHub's Registered past register_timeout_s: decide unregisters it,
\* so it is not supply. A runner on a machine that is not Up is unregistered too.
Supply == Count({m \in Machines : mstate[m] = "Up" /\ runner[m] \in {"Registered", "Idle"}})
Want0 == Count(Queued) - Supply

Fresh == {m \in Machines : /\ IsEph(m) /\ mstate[m] = "Up" /\ runner[m] = "None"
                           /\ ~seen[m] /\ age[m] = 0}
FreeKept ==
    IF Has("slot")
    THEN {m \in Machines : Kept(m) /\ mstate[m] = "Up" /\ runner[m] = "None"}
    ELSE {}
\* A Starting ephemeral machine past its TTL is deleted this tick, so it is not pending.
Pending == Count({m \in Machines : mstate[m] = "Starting" /\ Vis(m) /\ ~PastTtl(m)})
\* decide's want after registers and pending, without its saturating steps: each
\* step subtracts a non-negative amount, so the sign comes out the same.
WantStart == Want0 - Count(Fresh) - Count(FreeKept) - Pending
Short == MinUp - Count({m \in Machines : Kept(m) /\ Live(m) /\ Vis(m)})

\* Starts.take() with run::start re-reading the quote after every start, so the
\* price is the one for the next start.
Price == IF starts < Included THEN 0 ELSE 1
BudgetOK == Price = 0 \/ spent_today + Price <= PaidBudget
\* The restart wait outlasts every create sent before the crash and the
\* listing's lag behind it.
Orphans == \E m \in Machines : lag[m] = "Orphan"
\* The lost-create wait, HTTP_TIMEOUT and one tick, outlasts the listing's lag.
LostOnes == \E m \in Machines : lag[m] = "Lost"
StartOK == /\ BudgetOK /\ ToolBoxes < BoxCap
           /\ ~(FixKeptInflight /\ Orphans) /\ ~(FixLostCreate /\ LostOnes)
Pay == spent_today' = spent_today + Price
Paid == IF starts > Included THEN starts - Included ELSE 0

FreeIds == {m \in Machines : mstate[m] = "Gone" /\ runner[m] = "None"}

Finished(m) ==
    \/ /\ mstate[m] = "Up"
       /\ runner[m] \in {"None", "Offline"}
       /\ seen[m] \/ age[m] >= 1
    \/ mstate[m] = "Stopped"

\* run::observe sets last_busy for every machine with a Busy or Registered runner
\* before decide runs, so no decision sees memory older than the snapshot.
ObserveGuard(m) ==
    /\ runner[m] \in {"Registered", "Expired", "Busy"}
    /\ \/ IsEph(m) /\ ~seen[m]
       \/ idle[m]
Observed == \A m \in Machines : ~ObserveGuard(m)
\* A tick executes its stops and deletes before the next snapshot.
NoPlan == \A m \in Machines : plan[m] = "None"

-----------------------------------------------------------------------------
(* Tool actions, in decide's order. *)

UnregisterGuard(m) ==
    /\ Observed
    /\ NoPlan
    /\ runner[m] # "None"
    /\ runner[m] \in {"Expired", "Offline"} \/ mstate[m] # "Up"

\* A stopped kept machine in a pool without resume would never come back.
DeleteGuard(m) ==
    /\ Observed
    /\ plan[m] = "None"
    /\ mstate[m] # "Gone"
    /\ runner[m] # "Busy"
    /\ \/ IsEph(m) /\ (Finished(m) \/ PastTtl(m))
       \/ Kept(m) /\ mstate[m] = "Stopped" /\ ~Has("resume")

\* A fresh forked machine is filled before any free kept slot.
RegisterPick(m) == Want0 > 0 /\ (m \in Fresh \/ (m \in FreeKept /\ Fresh = {}))
RegisterGuard(m) == Observed /\ NoPlan /\ RegisterPick(m)

ResumeGuard(m) ==
    /\ Observed
    /\ NoPlan
    /\ Has("resume")
    /\ m \in StoppedKept
    /\ WantStart > 0 \/ Short > 0
    /\ StartOK
CanResume == \E m \in Machines : ResumeGuard(m)

\* A resume that fails here fails on the same budget and boxes, so a kept fork
\* only runs when no resume can.
ForkKeptGuard ==
    /\ Observed
    /\ NoPlan
    /\ Short > 0 \/ (Grows /\ WantStart > 0)
    /\ ~CanResume
    /\ StartOK
    /\ FreeIds # {}

ForkGuard(j) ==
    /\ Observed
    /\ NoPlan
    /\ Has("fork")
    /\ j \in Queued
    /\ WantStart > 0
    /\ ~CanResume
    /\ ~ForkKeptGuard
    /\ StartOK
    /\ FreeIds # {}
    /\ \A m \in Machines : mstate[m] # "Gone" /\ Vis(m) => job[m] # j

StopGuard(m) ==
    /\ Observed
    /\ plan[m] = "None"
    /\ Has("resume")
    /\ Kept(m)
    /\ mstate[m] = "Up"
    /\ runner[m] \notin {"Busy", "Registered", "Expired"}
    /\ idle[m]
    /\ UpKept - Count({n \in Machines : plan[n] = "Stop"}) > MinUp
    /\ WantStart <= 0
    /\ ~RegisterPick(m)

\* A stopped or deleted machine takes its runner offline, and the job that was
\* running there goes back to the queue. With FixStaleAct the tool deleted the
\* runner first, so none is left.
Drop(m) ==
    /\ runner' = [runner EXCEPT ![m] = IF @ = "None" \/ FixStaleAct THEN "None" ELSE "Offline"]
    /\ status' = [j \in Jobs |->
                    IF jobOn[j] = m /\ status[j] = "Running" THEN "Queued" ELSE status[j]]
    /\ jobOn' = [j \in Jobs |-> IF jobOn[j] = m THEN NoMachine ELSE jobOn[j]]

Unregister(m) ==
    /\ UnregisterGuard(m)
    /\ runner' = [runner EXCEPT ![m] = "None"]
    /\ UNCHANGED <<lag, mstate, job, keep, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran, faults>>

Delete(m) ==
    /\ DeleteGuard(m)
    /\ plan' = [plan EXCEPT ![m] = "Delete"]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age,
                   starts, spent_today, keys, ran, faults>>

Stop(m) ==
    /\ StopGuard(m)
    /\ plan' = [plan EXCEPT ![m] = "Stop"]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age,
                   starts, spent_today, keys, ran, faults>>

StopNow(m) ==
    /\ mstate' = [mstate EXCEPT ![m] = "Stopped"]
    /\ keep' = [keep EXCEPT ![m] = TRUE]
    /\ idle' = [idle EXCEPT ![m] = FALSE]
    /\ Drop(m)
    /\ UNCHANGED <<lag, job, seen, age, ran>>

DeleteNow(m) ==
    /\ mstate' = [mstate EXCEPT ![m] = "Gone"]
    /\ lag' = [lag EXCEPT ![m] = "Listed"]
    /\ job' = [job EXCEPT ![m] = NoJob]
    /\ keep' = [keep EXCEPT ![m] = FALSE]
    /\ seen' = [seen EXCEPT ![m] = FALSE]
    /\ idle' = [idle EXCEPT ![m] = FALSE]
    /\ age' = [age EXCEPT ![m] = 0]
    /\ ran' = [ran EXCEPT ![m] = FALSE]
    /\ Drop(m)

\* run::execute calls boat with what decide saw; the snapshot may be stale by now.
Execute(m) ==
    /\ plan[m] # "None"
    /\ plan' = [plan EXCEPT ![m] = "None"]
    /\ IF FixStaleAct /\ runner[m] = "Busy"
       THEN UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age, ran>>
       ELSE IF plan[m] = "Stop" THEN StopNow(m) ELSE DeleteNow(m)
    /\ UNCHANGED <<starts, spent_today, keys, faults>>

Register(m) ==
    /\ RegisterGuard(m)
    /\ runner' = [runner EXCEPT ![m] = "Registered"]
    /\ seen' = IF FixRegSeen /\ IsEph(m) THEN [seen EXCEPT ![m] = TRUE] ELSE seen
    /\ idle' = IF FixRegSeen THEN [idle EXCEPT ![m] = FALSE] ELSE idle
    /\ UNCHANGED <<lag, mstate, job, keep, status, jobOn, age, plan,
                   starts, spent_today, keys, ran, faults>>

\* The tool adds the quoted price whether or not boat counts the start.
Resume(m) ==
    /\ ResumeGuard(m)
    /\ mstate' = [mstate EXCEPT ![m] = "Starting"]
    /\ keep' = [keep EXCEPT ![m] = FALSE]
    /\ starts' = IF keep[m] THEN starts ELSE starts + 1
    /\ Pay
    /\ UNCHANGED <<lag, job, runner, status, jobOn, seen, idle, age, plan, keys, ran, faults>>

\* boat makes machine m for job j and counts the start; l is the listing state.
Create(m, j, l) ==
    /\ mstate' = [mstate EXCEPT ![m] = "Starting"]
    /\ lag' = [lag EXCEPT ![m] = l]
    /\ job' = [job EXCEPT ![m] = j]
    /\ age' = [age EXCEPT ![m] = 0]
    /\ seen' = [seen EXCEPT ![m] = FALSE]
    /\ starts' = starts + 1

NewMachine(m, j) == Create(m, j, IF FixListLag THEN "Known" ELSE "Hidden") /\ Pay

ForkKept ==
    /\ ForkKeptGuard
    /\ \E m \in FreeIds : NewMachine(m, NoJob)
    /\ UNCHANGED <<keep, runner, status, jobOn, idle, plan, keys, ran, faults>>

\* With the job key as Idempotency-Key, boat answers a reused key with the old,
\* deleted machine: nothing starts, and the tool still adds the price.
Fork(j) ==
    /\ ForkGuard(j)
    /\ IF FixForkKey \/ j \notin keys
       THEN /\ \E m \in FreeIds : NewMachine(m, j)
            /\ keys' = IF FixForkKey THEN keys ELSE keys \cup {j}
            /\ UNCHANGED <<keep, runner, status, jobOn, idle, plan, ran, faults>>
       ELSE /\ Pay
            /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age,
                           plan, starts, keys, ran, faults>>

Observe(m) ==
    /\ NoPlan
    /\ ObserveGuard(m)
    /\ seen' = [seen EXCEPT ![m] = @ \/ IsEph(m)]
    /\ idle' = [idle EXCEPT ![m] = FALSE]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, age, plan,
                   starts, spent_today, keys, ran, faults>>

-----------------------------------------------------------------------------
(* GitHub and boat. *)

Arrive(j) ==
    /\ status[j] = "Pending"
    /\ status' = [status EXCEPT ![j] = "Queued"]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran, faults>>

Assign(j, m) ==
    /\ status[j] = "Queued"
    /\ mstate[m] = "Up"
    /\ runner[m] = "Idle"
    /\ status' = [status EXCEPT ![j] = "Running"]
    /\ jobOn' = [jobOn EXCEPT ![j] = m]
    /\ runner' = [runner EXCEPT ![m] = "Busy"]
    /\ ran' = IF IsEph(m) THEN [ran EXCEPT ![m] = TRUE] ELSE ran
    /\ UNCHANGED <<lag, mstate, job, keep, seen, idle, age, plan,
                   starts, spent_today, keys, faults>>

\* A JIT runner exits after its job and GitHub removes it.
Finish(j) ==
    /\ status[j] = "Running"
    /\ status' = [status EXCEPT ![j] = "Done"]
    /\ jobOn' = [jobOn EXCEPT ![j] = NoMachine]
    /\ runner' = [runner EXCEPT ![jobOn[j]] = "None"]
    /\ UNCHANGED <<lag, mstate, job, keep, seen, idle, age, plan,
                   starts, spent_today, keys, ran, faults>>

\* A state change moves since, so age and the seen flag start over.
Boot(m) ==
    /\ mstate[m] = "Starting"
    /\ mstate' = [mstate EXCEPT ![m] = "Up"]
    /\ lag' = [lag EXCEPT ![m] = "Listed"]
    /\ age' = [age EXCEPT ![m] = 0]
    /\ seen' = [seen EXCEPT ![m] = FALSE]
    /\ idle' = [idle EXCEPT ![m] = FALSE]
    /\ UNCHANGED <<job, keep, runner, status, jobOn, plan,
                   starts, spent_today, keys, ran, faults>>

ComeOnline(m) ==
    /\ mstate[m] = "Up"
    /\ runner[m] = "Registered"
    /\ runner' = [runner EXCEPT ![m] = "Idle"]
    /\ UNCHANGED <<lag, mstate, job, keep, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran, faults>>

(* Time is abstract: a flag moves once its timeout has passed. The timing    *)
(* assumptions: the tool registers within a tick and GitHub assigns within   *)
(* seconds, both far inside idle_stop_s and register_timeout_s, so a machine *)
(* that could serve a queued job does not age into idle, and a fresh fork    *)
(* with demand waiting does not age past register_timeout_s.                 *)
TickIdle(m) ==
    /\ Kept(m)
    /\ mstate[m] = "Up"
    /\ ~idle[m]
    /\ runner[m] \in {"None", "Idle", "Offline"}
    /\ ~(Queued # {} /\ runner[m] \in {"None", "Idle"})
    /\ idle' = [idle EXCEPT ![m] = TRUE]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, age, plan,
                   starts, spent_today, keys, ran, faults>>

TickAge(m) ==
    /\ IsEph(m)
    /\ Live(m)
    /\ Vis(m)
    /\ age[m] < 2
    /\ ~(m \in Fresh /\ Want0 > 0)
    /\ age' = [age EXCEPT ![m] = @ + 1]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, plan,
                   starts, spent_today, keys, ran, faults>>

\* boat keeps a stopped machine's disk for 10 minutes.
TickKeep(m) ==
    /\ mstate[m] = "Stopped"
    /\ keep[m]
    /\ keep' = [keep EXCEPT ![m] = FALSE]
    /\ UNCHANGED <<lag, mstate, job, runner, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran, faults>>

-----------------------------------------------------------------------------
(* Faults, bounded by MaxFaults so that liveness can hold. *)

Fault == faults < MaxFaults /\ faults' = faults + 1

\* The job goes back to the queue and its runner is gone.
Reassign(j) ==
    /\ Fault
    /\ status[j] = "Running"
    /\ status' = [status EXCEPT ![j] = "Queued"]
    /\ jobOn' = [jobOn EXCEPT ![j] = NoMachine]
    /\ runner' = [runner EXCEPT ![jobOn[j]] = "None"]
    /\ UNCHANGED <<lag, mstate, job, keep, seen, idle, age, plan,
                   starts, spent_today, keys, ran>>

\* The resume then costs a counted start.
LoseKept(m) ==
    /\ Fault
    /\ keep[m]
    /\ keep' = [keep EXCEPT ![m] = FALSE]
    /\ UNCHANGED <<lag, mstate, job, runner, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran>>

\* A JIT runner that never comes online; Expired means its timeout passed.
NeverOnline(m) ==
    /\ Fault
    /\ runner[m] = "Registered"
    /\ runner' = [runner EXCEPT ![m] = "Expired"]
    /\ UNCHANGED <<lag, mstate, job, keep, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran>>

RunnerOffline(m) ==
    /\ Fault
    /\ mstate[m] = "Up"
    /\ runner[m] = "Idle"
    /\ runner' = [runner EXCEPT ![m] = "Offline"]
    /\ UNCHANGED <<lag, mstate, job, keep, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran>>

\* With FixStaleAct, deleting the runner fails on a network error, a rate limit
\* or a 5xx, and the tool skips the Stop or Delete this tick. A Busy runner is
\* refused every time, in Execute.
RefuseUnregister(m) ==
    /\ Fault
    /\ FixStaleAct
    /\ plan[m] # "None"
    /\ runner[m] \notin {"None", "Busy"}
    /\ plan' = [plan EXCEPT ![m] = "None"]
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age,
                   starts, spent_today, keys, ran>>

\* A create times out, fails in transport or gets a 5xx, and boat made the
\* machine anyway. run::start adds the price only on a definite answer, so
\* without FixLostSpend the tool does not count the start boat counted. A lost
\* create of a machine boat never made only delays the next start.
LostCreate ==
    /\ Fault
    /\ \/ ForkKeptGuard /\ \E m \in FreeIds : Create(m, NoJob, "Lost") /\ UNCHANGED keys
       \/ \E j \in Jobs : /\ ForkGuard(j)
                          /\ FixForkKey \/ j \notin keys
                          /\ \E m \in FreeIds : Create(m, j, "Lost")
                          /\ keys' = IF FixForkKey THEN keys ELSE keys \cup {j}
    /\ spent_today' = IF FixLostSpend THEN spent_today + Price ELSE spent_today
    /\ UNCHANGED <<keep, runner, status, jobOn, idle, plan, ran>>

\* boat answers 429 or 503: nothing starts, nothing is counted, and the tool
\* tries again on a later tick.
RefuseStart ==
    /\ Fault
    /\ CanResume \/ ForkKeptGuard \/ \E j \in Jobs : ForkGuard(j)
    /\ UNCHANGED <<lag, mstate, job, keep, runner, status, jobOn, seen, idle, age, plan,
                   starts, spent_today, keys, ran>>

(* A restart between two actions loses the tick's unexecuted plan. Every     *)
(* machine's since and last_busy start over at first sight, and the online   *)
(* set is lost, so a runner that went offline reads as Registered past its   *)
(* timeout. spent_today cannot be recovered from boat. Every fork key is a   *)
(* fresh UUID v4, so the tool cannot find a create that boat has not listed  *)
(* yet: that machine becomes an Orphan. A create still in flight at the      *)
(* crash is the same case, with boat counting the start a little later.     *)
Crash ==
    /\ Fault
    /\ seen' = [m \in Machines |-> FixCrashSeen /\ IsEph(m) /\ mstate[m] = "Up"]
    /\ idle' = [m \in Machines |-> FALSE]
    /\ age' = [m \in Machines |-> 0]
    /\ plan' = [m \in Machines |-> "None"]
    /\ runner' = [m \in Machines |-> IF runner[m] = "Offline" THEN "Expired" ELSE runner[m]]
    /\ spent_today' = IF FixCrashSpend THEN PaidBudget ELSE 0
    /\ lag' = [m \in Machines |-> IF lag[m] = "Listed" THEN "Listed" ELSE "Orphan"]
    /\ UNCHANGED <<mstate, job, keep, status, jobOn, starts, keys, ran>>

-----------------------------------------------------------------------------

Init ==
    /\ mstate = [m \in Machines |-> IF m \in InitStopped THEN "Stopped" ELSE "Gone"]
    /\ job = [m \in Machines |-> NoJob]
    /\ keep = [m \in Machines |-> FALSE]
    /\ runner = [m \in Machines |-> "None"]
    /\ status = [j \in Jobs |-> "Pending"]
    /\ jobOn = [j \in Jobs |-> NoMachine]
    /\ seen = [m \in Machines |-> FALSE]
    /\ idle = [m \in Machines |-> FALSE]
    /\ age = [m \in Machines |-> 0]
    /\ plan = [m \in Machines |-> "None"]
    /\ starts = 0
    /\ spent_today = 0
    /\ keys = {}
    /\ ran = [m \in Machines |-> FALSE]
    /\ lag = [m \in Machines |-> "Listed"]
    /\ faults = 0

ToolStep ==
    \/ \E m \in Machines :
          \/ Observe(m)
          \/ Unregister(m)
          \/ Delete(m)
          \/ Stop(m)
          \/ Execute(m)
          \/ Register(m)
          \/ Resume(m)
    \/ ForkKept
    \/ \E j \in Jobs : Fork(j)

EnvStep ==
    \/ \E j \in Jobs :
          \/ Arrive(j)
          \/ Finish(j)
          \/ \E m \in Machines : Assign(j, m)
    \/ \E m \in Machines :
          \/ Boot(m)
          \/ ComeOnline(m)
          \/ TickIdle(m)
          \/ TickAge(m)
          \/ TickKeep(m)

FaultStep ==
    \/ \E j \in Jobs : Reassign(j)
    \/ \E m \in Machines :
          \/ LoseKept(m)
          \/ NeverOnline(m)
          \/ RunnerOffline(m)
          \/ RefuseUnregister(m)
    \/ RefuseStart
    \/ LostCreate
    \/ Crash

Next == ToolStep \/ EnvStep \/ FaultStep

\* decide returns every enabled action each tick, so each one is weakly fair.
\* Time, boot, runner start-up and GitHub's dispatcher are weakly fair too.
\* Arrivals and faults get no fairness.
Fairness ==
    /\ \A m \in Machines :
          /\ WF_vars(Observe(m))
          /\ WF_vars(Unregister(m))
          /\ WF_vars(Delete(m))
          /\ WF_vars(Stop(m))
          /\ WF_vars(Execute(m))
          /\ WF_vars(Register(m))
          /\ WF_vars(Resume(m))
          /\ WF_vars(Boot(m))
          /\ WF_vars(ComeOnline(m))
          /\ WF_vars(TickIdle(m))
          /\ WF_vars(TickAge(m))
          /\ WF_vars(TickKeep(m))
    /\ WF_vars(ForkKept)
    /\ \A j \in Jobs :
          /\ WF_vars(Fork(j))
          /\ WF_vars(Finish(j))
          /\ WF_vars(\E m \in Machines : Assign(j, m))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Properties, numbered as in docs/design.md. *)

TypeOK ==
    /\ mstate \in [Machines -> {"Starting", "Up", "Stopped", "Gone"}]
    /\ job \in [Machines -> Jobs \cup {NoJob}]
    /\ keep \in [Machines -> BOOLEAN]
    /\ runner \in [Machines -> {"None", "Registered", "Expired", "Idle", "Busy", "Offline"}]
    /\ status \in [Jobs -> {"Pending", "Queued", "Running", "Done"}]
    /\ jobOn \in [Jobs -> Machines \cup {NoMachine}]
    /\ seen \in [Machines -> BOOLEAN]
    /\ idle \in [Machines -> BOOLEAN]
    /\ age \in [Machines -> 0..2]
    /\ plan \in [Machines -> {"None", "Stop", "Delete"}]
    /\ starts \in Nat
    /\ spent_today \in Nat
    /\ keys \subseteq Jobs
    /\ ran \in [Machines -> BOOLEAN]
    /\ lag \in [Machines -> {"Listed", "Known", "Hidden", "Lost", "Orphan"}]
    /\ faults \in 0..MaxFaults

\* 1
NoBusyStop ==
    [][\A m \in Machines :
         (Live(m) /\ mstate'[m] \in {"Stopped", "Gone"}) => runner[m] # "Busy"]_vars

\* 2: a busy runner keeps its job or goes away; it never goes back to Idle.
OneJobPerRunner ==
    [][\A m \in Machines :
         runner[m] = "Busy" => runner'[m] \in {"Busy", "None", "Offline"}]_vars

\* 3
MaxBoxes == Boxes <= BoxCap

\* 4: boat's own count of paid starts, against the cap, across restarts.
StartBudget == Paid <= PaidBudget

\* 5: at most one live fork per job; a key reused after its machine is Gone is
\* allowed. Ephemeral machines are never stopped, so not Gone means live.
NoDoubleFork ==
    \A a, b \in Machines :
        (a # b /\ mstate[a] # "Gone" /\ mstate[b] # "Gone" /\ IsEph(a)) => job[a] # job[b]

\* 6
EphemeralIsolation ==
    [][Placement = {"fork"} =>
         \A m \in Machines : (runner[m] # "Busy" /\ runner'[m] = "Busy") => ~ran[m]]_vars

\* 6 for job forks in any placement; stronger than the design asks.
ForkIsolation ==
    [][\A m \in Machines : (runner[m] # "Busy" /\ runner'[m] = "Busy") => ~ran[m]]_vars

\* 7
NoLeak ==
    \A m \in Machines :
        (IsEph(m) /\ Live(m) /\ (status[job[m]] = "Done" \/ PastTtl(m)))
            ~> (mstate[m] = "Gone")

\* No placement step can serve a job: nothing is up or starting, and the tool's
\* budget or max_boxes allows no start. A pool with resume and no fork forks kept
\* machines for demand, so either step can start one.
CanStart ==
    /\ Boxes < BoxCap
    /\ BudgetOK
    /\ Has("fork") \/ Has("resume")
Blocked == ~CanStart /\ \A m \in Machines : ~Live(m)

\* Liveness 1
NoStarvation ==
    \A j \in Jobs : (status[j] = "Queued") ~> (status[j] # "Queued" \/ Blocked)

\* Liveness 2
IdleStops ==
    \A m \in Machines :
        (Has("resume") /\ Kept(m) /\ mstate[m] = "Up" /\ runner[m] # "Busy")
            ~> (mstate[m] # "Up" \/ runner[m] = "Busy" \/ UpKept <= MinUp)

\* Liveness 3: arrivals and faults are bounded, so "no new jobs" holds eventually.
Settles == <>[](~ENABLED ToolStep)

=============================================================================

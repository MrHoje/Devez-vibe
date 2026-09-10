// Runs the sibling host's real readers, callbacks and completion gate in an
// isolated console harness. UI dispatch/timers are controlled to replay races.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const host = path.resolve(process.argv[2] ?? path.join(repo, '..', 'Devez-code'));
const source = fs.readFileSync(path.join(host, 'MainWindow.xaml.cs'), 'utf8');
function between(start, end) {
  const first = source.indexOf(start);
  const last = source.indexOf(end, first + start.length);
  if (first < 0 || last < 0) throw new Error(`호스트 코드 구간을 찾을 수 없음: ${start}`);
  return source.slice(first, last);
}
const callbacks = between('        _devezVibeState.BusyChanged +=', '        _devezVibeState.SessionChanged +=');
const completion = between('    private void NotifyIfSessionFinished(', '    private void EmitSessionFinished(');
const waitingNotification = between('    private void NotifyIfSessionWaiting(', '    private void EmitSessionWaiting(');
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'devez-question-host-'));
fs.copyFileSync(path.join(host, 'Services', 'DevezVibeStateService.cs'), path.join(scratch, 'DevezVibeStateService.cs'));
fs.writeFileSync(path.join(scratch, 'Harness.csproj'), `<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net9.0</TargetFramework>
  <ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable></PropertyGroup>
</Project>`);
fs.writeFileSync(path.join(scratch, 'Program.cs'), `
using System.Reflection;
using DevezCode.Services;

namespace System.Windows.Threading {
    public sealed class DispatcherTimer {
        public TimeSpan Interval { get; set; }
        public event EventHandler? Tick;
        bool active;
        public void Start() => active = true;
        public void Stop() => active = false;
        public void Fire() { if (active) Tick?.Invoke(this, EventArgs.Empty); }
    }
}
namespace DevezCode.Services { public static class DiagLog { public static void Write(string text) { } } }
sealed class SessionItem { public string Id = ""; public bool IsBusy, IsWaitingChoice; }
sealed class UiQueue {
    readonly List<Action> pending = new();
    public void InvokeAsync(Action action) { lock (pending) pending.Add(action); }
    public void Drain(bool reverse) {
        Action[] actions;
        lock (pending) { actions = pending.ToArray(); pending.Clear(); }
        if (reverse) Array.Reverse(actions);
        foreach (var action in actions) action();
    }
}
sealed class Pane { public void NotifyModelEffortChanged(string room) { } }
sealed class Usage { public void RequestRefreshSoon() { } }
sealed class Harness : IDisposable {
    readonly DevezVibeStateService _devezVibeState = new();
    readonly HashSet<string> _devezVibeQuietRooms = new(), _devezVibeCompactingRooms = new();
    readonly Dictionary<SessionItem, System.Windows.Threading.DispatcherTimer> _finishDebounce = new();
    readonly Dictionary<SessionItem, System.Windows.Threading.DispatcherTimer> _waitingNotificationDebounce = new();
    readonly List<Pane> _panes = new();
    readonly Usage _usageApi = new();
    readonly UiQueue Dispatcher = new();
    readonly SessionItem session = new() { Id = "question-test-" + Guid.NewGuid().ToString("N") };
    const int FinishSettleMs = 1500;
    int finished, waitingNotifications;
    static readonly string root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "DevezCode", "devezvibe");
    SessionItem FindOwnedSession(string id, string agent, string kind) => session;
    void MarkSessionActivity(string room) { }
    void UpdateSessionBusyDisplay() { }
    void EmitSessionWaiting(SessionItem s) => waitingNotifications++;
    void EmitSessionFinished(SessionItem s) => finished++;
    public Harness() {
${callbacks}
    }
${completion}
${waitingNotification}
    string FilePath(string kind) => Path.Combine(root, kind, session.Id + ".txt");
    void Emit(string kind) => typeof(DevezVibeStateService).GetMethod(
        kind == "busy" ? "EmitBusy" : "EmitWaiting", BindingFlags.Instance | BindingFlags.NonPublic)!
        .Invoke(_devezVibeState, new object[] { FilePath(kind) });
    void Write(string kind, string status, bool emit = true) {
        var file = FilePath(kind);
        Directory.CreateDirectory(Path.GetDirectoryName(file)!);
        File.WriteAllText(file + ".tmp", status);
        File.Move(file + ".tmp", file, true);
        if (emit) Emit(kind);
    }
    void Settle(bool reverse) {
        Dispatcher.Drain(reverse);
        foreach (var timer in _finishDebounce.Values.ToArray()) timer.Fire();
        foreach (var timer in _waitingNotificationDebounce.Values.ToArray()) timer.Fire();
    }
    void Check(bool busy, bool waiting, int completions) {
        if (session.IsBusy != busy || session.IsWaitingChoice != waiting || finished != completions)
            throw new Exception($"대기 상태 불일치: busy={session.IsBusy}, waiting={session.IsWaitingChoice}, completed={finished}");
    }
    public void Run(bool reverse, bool cancel) {
        Write("waiting", "ready"); Write("busy", "running"); Settle(reverse);
        Check(true, false, 0);
        Write("waiting", "waiting"); Emit("busy"); Settle(reverse);
        Check(true, true, 0);
        // The provider has stopped, but the corrected producer keeps running.
        for (int i = 0; i < 30; i++) { Emit("busy"); Emit("waiting"); Settle(reverse); Check(true, true, 0); }
        if (waitingNotifications != 1) throw new Exception("대기 알림이 누락되거나 중복됨");
        Write("waiting", "ready");
        if (!cancel) { Settle(reverse); Check(true, false, 0); }
        Write("busy", "idle"); Settle(reverse);
        Check(false, false, 1);
        for (int i = 0; i < 10; i++) { Emit("waiting"); Emit("busy"); Settle(reverse); Check(false, false, 1); }
    }
    public void OldContractKeepsWaiting(bool reverse) {
        Write("busy", "running"); Settle(false);
        Write("waiting", "waiting"); Settle(false);
        Write("busy", "idle"); Settle(false);
        Check(false, true, 0);
        Write("waiting", "ready"); Emit("busy"); Settle(reverse);
        Check(false, false, 1);
    }
    public void ReorderedIdleIsGuarded() {
        Write("busy", "running"); Settle(false);
        Write("busy", "idle");
        Write("busy", "running"); Write("waiting", "waiting");
        Settle(true); // Stale idle callback arrives last; the disk gate must suppress completion.
        Check(true, true, 0);
    }
    public void ReorderedWaitingDoesNotReopenQuestion() {
        Write("busy", "running"); Settle(false);
        Write("waiting", "waiting"); Write("waiting", "ready");
        Settle(true);
        Check(true, false, 0);
        if (waitingNotifications != 0) throw new Exception("이미 답한 질문의 대기 알림이 발행됨");
    }
    public void QuietActivitiesNeverComplete() {
        Write("waiting", "ready"); Write("busy", "loading"); Settle(false);
        Check(false, false, 0);
        Write("busy", "idle"); Settle(false); Check(false, false, 0);
        Write("busy", "compacting"); Settle(false); Check(true, false, 0);
        Write("busy", "idle"); Settle(false); Check(false, false, 0);
    }
    public void FastTurnStillCompletes(bool reverse) {
        Write("waiting", "ready"); Settle(false);
        Write("busy", "running");
        for (int i = 0; i < 30; i++) Emit("busy");
        Write("busy", "idle");
        Settle(reverse);
        Check(false, false, 1);
        Emit("busy"); Settle(false); Check(false, false, 1);
    }
    public void LockedActivityDoesNotApplyStaleIdle() {
        Write("busy", "running"); Write("waiting", "ready"); Settle(false);
        Write("busy", "idle"); Write("busy", "running"); Write("waiting", "waiting");
        using (var locked = new FileStream(FilePath("busy"), FileMode.Open, FileAccess.ReadWrite, FileShare.None)) {
            Settle(true);
            Check(true, true, 0);
        }
    }
    public void LockedWaitingDoesNotReopenAnsweredQuestion() {
        Write("busy", "running"); Write("waiting", "ready"); Settle(false);
        Write("waiting", "waiting"); Write("waiting", "ready");
        using (var locked = new FileStream(FilePath("waiting"), FileMode.Open, FileAccess.ReadWrite, FileShare.None)) {
            Settle(true);
            Check(true, false, 0);
            if (waitingNotifications != 0) throw new Exception("읽기 실패로 이미 답한 질문을 다시 알림");
        }
    }
    public void MalformedStateKeepsLastKnownQuestion() {
        Write("busy", "running"); Write("waiting", "waiting"); Settle(false);
        Write("busy", "broken"); Write("waiting", "broken"); Settle(false);
        Check(true, true, 0);
        if (_devezVibeState.ReadRoomActivity(session.Id)?.Busy != true ||
            _devezVibeState.ReadRoomWaiting(session.Id) != true) throw new Exception("잘못된 파일이 정상 상태를 덮음");
        Write("busy", "idle"); Write("waiting", "ready"); Settle(false);
        Check(false, false, 1);
    }
    public void RealWatcherDeliversAtomicRenames() {
        Write("busy", "idle"); Write("waiting", "ready"); Settle(false);
        var factory = typeof(DevezVibeStateService).GetMethod("MakeWatcher", BindingFlags.Static | BindingFlags.NonPublic)!;
        FileSystemWatcher Watch(string kind) => (FileSystemWatcher)factory.Invoke(null, new object[] {
            Path.GetDirectoryName(FilePath(kind))!, (Action<string>)(file => {
                if (file == FilePath(kind)) Emit(kind);
            })
        })!;
        using var busyWatcher = Watch("busy");
        using var waitingWatcher = Watch("waiting");
        void Await(bool busy, bool waiting, int completions) {
            var until = DateTime.UtcNow.AddSeconds(5);
            while (DateTime.UtcNow < until) {
                Settle(false);
                if (session.IsBusy == busy && session.IsWaitingChoice == waiting && finished == completions) return;
                Thread.Sleep(10);
            }
            Check(busy, waiting, completions);
        }
        Write("busy", "running", emit: false); Await(true, false, 0);
        Write("waiting", "waiting", emit: false); Await(true, true, 0);
        Write("waiting", "ready", emit: false); Await(true, false, 0);
        Write("busy", "idle", emit: false); Await(false, false, 1);
    }
    public void Dispose() {
        _devezVibeState.Dispose();
        foreach (var kind in new[] { "busy", "waiting" }) File.Delete(FilePath(kind));
    }
    static void Main() {
        using (var watcher = new Harness()) watcher.RealWatcherDeliversAtomicRenames();
        using (var invalid = new Harness()) invalid.MalformedStateKeepsLastKnownQuestion();
        using (var locked = new Harness()) locked.LockedActivityDoesNotApplyStaleIdle();
        using (var locked = new Harness()) locked.LockedWaitingDoesNotReopenAnsweredQuestion();
        foreach (bool reverse in new[] { false, true })
            using (var fast = new Harness()) fast.FastTurnStillCompletes(reverse);
        foreach (bool reverse in new[] { false, true })
            using (var old = new Harness()) old.OldContractKeepsWaiting(reverse);
        foreach (bool reverse in new[] { false, true })
            foreach (bool cancel in new[] { false, true })
                using (var test = new Harness()) test.Run(reverse, cancel);
        using (var race = new Harness()) race.ReorderedIdleIsGuarded();
        using (var race = new Harness()) race.ReorderedWaitingDoesNotReopenQuestion();
        using (var quiet = new Harness()) quiet.QuietActivitiesNeverComplete();
        Console.WriteLine("호스트 수신 15개 시나리오 통과: 짧은 턴·중복·역순·파일 잠금·손상·실제 파일 감시·질문 대기·완료 알림");
    }
}
`);
const result = spawnSync('dotnet', ['run', '--project', path.join(scratch, 'Harness.csproj'), '--verbosity', 'quiet'], { encoding: 'utf8', timeout: 120000 });
process.stdout.write(result.stdout ?? '');
process.stderr.write(result.stderr ?? '');
console.log(`검증 산출물: ${scratch}`);
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;

const std = @import("std");

const Kind = enum { build, test };

const Partition = struct {
    id: []const u8,
    kind: Kind,
    src: []const u8,
    deps: []const []const u8,
    out: []const u8,
};

pub fn main() !void {
    var gpa = std.heap.GeneralPurposeAllocator(.{}){};
    defer _ = gpa.deinit();
    const allocator = gpa.allocator();

    const args = try std.process.argsAlloc(allocator);
    defer std.process.argsFree(allocator, args);

    if (args.len <= 1) {
        try std.io.getStdOut().writer().print("FrostBuild Zig skeleton\ncommands:\n  frost plan --workspace sample\n  frost build --workspace sample\n", .{});
        return;
    }

    const cmd = args[1];
    if (std.mem.eql(u8, cmd, "plan")) {
        try std.io.getStdOut().writer().print("plan: TODO parse graph and prune partitions\n", .{});
    } else if (std.mem.eql(u8, cmd, "build")) {
        try std.io.getStdOut().writer().print("build: TODO execute selected DAG with CAS/action cache\n", .{});
    } else {
        try std.io.getStdOut().writer().print("unknown command: {s}\n", .{cmd});
    }
}

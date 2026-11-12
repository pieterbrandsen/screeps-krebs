// not a mirror: the CLI realm's module table (one per CLI connection, ARCHITECTURE.md §8.6). The host evaluates
//     <this file>
//     __chitinDefine("<path under js/>", function(module, exports, require) { <source> });   per CLI file + lodash
//     __chitinDefine("common/constants.js", …)
// then calls `__cliStart(natives)`, which returns `run(line) -> output text`, and deletes these globals.
// `require` resolves the specifiers the vanilla CLI files use: 'lodash', 'q', 'util', '@screeps/common',
// '../utils', '../strongholds', './<file>'. The natives (`__cli`, captured by the modules while they load):
//   docs(collection) -> JSON text of its docs | undefined           (storage reads, storage.js)
//   write(collection, writesJson) -> JSON text of inserted docs     (storage writes; throws Error(message))
//   env(op, argsJson) -> JSON text | undefined                      (storage.env)
//   call(name, argsJson) -> JSON text                               (server-side commands; throws Error(message))
//   out(text)                                                        (a line of output to the connection)
(function (G) {
    'use strict';
    var defs = Object.create(null), cache = Object.create(null);

    G.__chitinDefine = function (path, fn) {
        defs[path] = fn;
    };

    // q, as the CLI files use it: when/reject/all/defer over native promises
    var q = function (v) { return Promise.resolve(v); };
    q.when = function (v) { return Promise.resolve(v); };
    q.reject = function (e) { return Promise.reject(e); };
    q.all = function (a) { return Promise.all(a); };
    q.defer = function () {
        var d = {};
        d.promise = new Promise(function (res, rej) { d.resolve = res; d.reject = rej; });
        return d;
    };
    q.ninvoke = function (obj, name) {
        var args = Array.prototype.slice.call(arguments, 2);
        return new Promise(function (res, rej) {
            obj[name].apply(obj, args.concat(function (e, r) { if (e) rej(e); else res(r); }));
        });
    };

    // backend/lib/strongholds.js's entry points: the world-side code is the engine's (cron::strongholds)
    function strongholds(natives) {
        return {
            templates: JSON.parse(natives.call('strongholdTemplates', '[]')),
            spawnStronghold: function (roomName, opts) {
                return new Promise(function (res, rej) {
                    try { res(JSON.parse(natives.call('spawnStronghold', JSON.stringify([roomName, opts])))); } catch (e) { rej(e.message); }
                });
            },
            expandStronghold: function (invaderCore) {
                return new Promise(function (res, rej) {
                    try { res(JSON.parse(natives.call('expandStronghold', JSON.stringify([invaderCore && invaderCore._id])))); } catch (e) { rej(e.message); }
                });
            },
        };
    }

    var builtins = {
        q: q,
    };

    function resolve(from, spec) {
        if (spec === 'lodash') return 'vendor/lodash.js';
        if (spec === '@screeps/common') return 'cli/common.js';
        if (spec === '@screeps/common/lib/constants') return 'common/constants.js';
        if (spec === '../utils') return 'cli/utils.js';
        if (spec === '../strongholds') return 'chitin:strongholds';
        if (spec === 'util') return 'cli/inspect.js';
        if (spec.indexOf('./') === 0) return 'cli/' + spec.slice(2).replace(/\.js$/, '') + '.js';
        return spec;
    }

    function load(path) {
        if (path in cache) return cache[path].exports;
        if (path in builtins) return builtins[path];
        if (path === 'chitin:strongholds') return (builtins[path] = strongholds(G.__cli));
        var fn = defs[path];
        if (!fn) throw new Error('Cannot find module "' + path + '"');
        var module = { exports: {} };
        cache[path] = module;
        // lodash 3 finds its realm through `this` when there is no `global`/`self`/`window`
        fn.call(path === 'vendor/lodash.js' ? G : module.exports, module, module.exports, function (spec) { return load(resolve(path, spec)); });
        return module.exports;
    }

    G.__cliStart = function (natives) {
        G.__cli = natives;
        var sandbox = load('cli/sandbox.js');
        var run;
        if (natives.launcher) {
            // screeps-launcher's HTTP CLI (`POST /cli`, its cli.js): the whole body is one expression, its result
            // awaited and written once, a string as it is and anything else through util.inspect (a falsy value
            // included), a throw as `Error: <stack>`; each write ends with "\n". The sandbox's globals are set up
            // as for a line session, with `print` writing through the same callback.
            var util = load('cli/inspect.js');
            var cb = function (data) { natives.out(data + '\n'); };
            sandbox.create(cb);
            run = function (command) {
                var result;
                try {
                    result = (0, eval)(command);
                } catch (e) {
                    cb('Error: ' + ((e && e.stack) || e));
                    return;
                }
                Promise.resolve(result).then(
                    function (v) { cb(typeof v !== 'string' ? '' + util.inspect(v) : '' + v); },
                    function (e) { cb('Error: ' + ((e && e.stack) || e)); });
            };
        } else {
            var writer = load('cli/server.js').writer(natives.out);
            run = sandbox.create(writer);
        }
        delete G.__cli;
        delete G.__chitinDefine;
        delete G.__cliStart;
        return run;
    };
})(globalThis);

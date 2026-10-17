import json,sys
d=json.load(open(sys.argv[1]))
el=d['elapsed_nanos']/1e9
print(f"elapsed {el:.2f}s")
for side in d['sides']:
    print('==',side['side'])
    for r in side['roles']:
        n=r['threads']
        tot=sum(s['nanos'] for s in r['states'])
        busy=[(s['state'],s['nanos']) for s in r['states'] if not s['wait']]
        waits=sorted([(s['state'],s['nanos']) for s in r['states'] if s['wait']],key=lambda x:-x[1])[:2]
        b=sum(x[1] for x in busy)
        print(f"  {r['role']:<28} x{n:<3} busy {b/1e9:6.2f}s ({b/max(tot,1)*100:4.0f}%)  " + ", ".join(f"{s}:{ns/1e9:.2f}s" for s,ns in busy if ns/1e9>0.05) + " | waits " + ", ".join(f"{s}:{ns/1e9:.2f}s" for s,ns in waits))

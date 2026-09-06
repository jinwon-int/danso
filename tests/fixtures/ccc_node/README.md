agent_runtime.py is the unchanged provider-neutral contract from jinwon-int/ccc-node
commit cad3eddd374c035113f12cd66094f28bdcc0f73d, bridge/core/agent_runtime.py.
It is vendored only for offline integration tests. Set CCC_NODE_SOURCE to test
against another checkout. Production imports the installed ccc-node package.

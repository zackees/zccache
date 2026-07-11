# zccache-test-support

Test-only filesystem fixtures. Builders provision disposable Windows VHDX,
Linux loop filesystems, and macOS disk images when prerequisites are present.
Unavailable rows return a named skip reason; they never silently pass.

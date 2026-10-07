	.att_syntax
	.file	"lib.ab282076ef658bae-cgu.0"
	.section	.text._RNvCseH3VtyqVVms_3lib11copy_offset,"ax",@progbits
	.globl	_RNvCseH3VtyqVVms_3lib11copy_offset
	.prefalign	4, .Lfunc_end0, nop
	.type	_RNvCseH3VtyqVVms_3lib11copy_offset,@function
_RNvCseH3VtyqVVms_3lib11copy_offset:
	cmpq	$4, %rdx
	jb	.LBB0_7
	cmpq	$128, %rdx
	movl	$128, %r8d
	cmovbq	%rdx, %r8
	leaq	-3(%r8), %rcx
	addq	$-4, %r8
	movl	%ecx, %eax
	andl	$3, %eax
	movl	$3, %edx
	cmpq	$3, %r8
	jb	.LBB0_5
	andq	$-4, %rcx
	negq	%rcx
	.p2align	4
.LBB0_3:
	movq	-24(%rdi,%rdx,8), %r8
	movq	%r8, -24(%rsi,%rdx,8)
	movq	-16(%rdi,%rdx,8), %r8
	movq	%r8, -16(%rsi,%rdx,8)
	movq	-8(%rdi,%rdx,8), %r8
	movq	%r8, -8(%rsi,%rdx,8)
	movq	(%rdi,%rdx,8), %r8
	movq	%r8, (%rsi,%rdx,8)
	leaq	(%rcx,%rdx), %r8
	addq	$4, %r8
	addq	$4, %rdx
	cmpq	$3, %r8
	jne	.LBB0_3
	testq	%rax, %rax
	je	.LBB0_7
.LBB0_5:
	leaq	(%rsi,%rdx,8), %rcx
	addq	$-24, %rcx
	leaq	(%rdi,%rdx,8), %rdx
	addq	$-24, %rdx
	xorl	%esi, %esi
	.p2align	4
.LBB0_6:
	movq	(%rdx,%rsi,8), %rdi
	movq	%rdi, (%rcx,%rsi,8)
	incq	%rsi
	cmpq	%rsi, %rax
	jne	.LBB0_6
.LBB0_7:
	retq
.Lfunc_end0:
	.size	_RNvCseH3VtyqVVms_3lib11copy_offset, .Lfunc_end0-_RNvCseH3VtyqVVms_3lib11copy_offset

	.section	.text._RNvCseH3VtyqVVms_3lib11read_offset,"ax",@progbits
	.globl	_RNvCseH3VtyqVVms_3lib11read_offset
	.prefalign	4, .Lfunc_end1, nop
	.type	_RNvCseH3VtyqVVms_3lib11read_offset,@function
_RNvCseH3VtyqVVms_3lib11read_offset:
	cmpq	$3, %rdx
	jae	.LBB1_1
	leaq	(%rsi,%rdx,8), %rax
	movq	(%rax), %rax
	retq
.LBB1_1:
	addq	$-3, %rdx
	cmpq	$127, %rdx
	ja	.LBB1_5
	leaq	(%rdi,%rdx,8), %rax
	movq	(%rax), %rax
	retq
.LBB1_5:
	pushq	%rax
	leaq	.Lanon.718f62d177c65bc51b0cda06dab40e69.1(%rip), %rax
	movl	$128, %esi
	movq	%rdx, %rdi
	movq	%rax, %rdx
	callq	*_RNvNtCshgE7ic3rT0S_4core9panicking18panic_bounds_check@GOTPCREL(%rip)
.Lfunc_end1:
	.size	_RNvCseH3VtyqVVms_3lib11read_offset, .Lfunc_end1-_RNvCseH3VtyqVVms_3lib11read_offset

	.section	.text._RNvCseH3VtyqVVms_3lib13copy_wasteful,"ax",@progbits
	.globl	_RNvCseH3VtyqVVms_3lib13copy_wasteful
	.prefalign	4, .Lfunc_end2, nop
	.type	_RNvCseH3VtyqVVms_3lib13copy_wasteful,@function
_RNvCseH3VtyqVVms_3lib13copy_wasteful:
	cmpq	$4, %rdx
	jb	.LBB2_1
	cmpq	$128, %rdx
	movl	$128, %eax
	cmovbq	%rdx, %rax
	addq	$24, %rsi
	addq	$24, %rdi
	movl	%eax, %eax
	leaq	-24(,%rax,8), %rdx
	movq	%rdi, %rax
	movq	%rsi, %rdi
	movq	%rax, %rsi
	jmpq	*memcpy@GOTPCREL(%rip)
.LBB2_1:
	retq
.Lfunc_end2:
	.size	_RNvCseH3VtyqVVms_3lib13copy_wasteful, .Lfunc_end2-_RNvCseH3VtyqVVms_3lib13copy_wasteful

	.section	.text._RNvCseH3VtyqVVms_3lib13read_wasteful,"ax",@progbits
	.globl	_RNvCseH3VtyqVVms_3lib13read_wasteful
	.prefalign	4, .Lfunc_end3, nop
	.type	_RNvCseH3VtyqVVms_3lib13read_wasteful,@function
_RNvCseH3VtyqVVms_3lib13read_wasteful:
	cmpq	$3, %rdx
	jb	.LBB3_2
	movq	%rdi, %rsi
	cmpq	$128, %rdx
	jae	.LBB3_3
.LBB3_2:
	movq	(%rsi,%rdx,8), %rax
	retq
.LBB3_3:
	pushq	%rax
	leaq	.Lanon.718f62d177c65bc51b0cda06dab40e69.2(%rip), %rax
	movl	$128, %esi
	movq	%rdx, %rdi
	movq	%rax, %rdx
	callq	*_RNvNtCshgE7ic3rT0S_4core9panicking18panic_bounds_check@GOTPCREL(%rip)
.Lfunc_end3:
	.size	_RNvCseH3VtyqVVms_3lib13read_wasteful, .Lfunc_end3-_RNvCseH3VtyqVVms_3lib13read_wasteful

	.type	.Lanon.718f62d177c65bc51b0cda06dab40e69.0,@object
	.section	.rodata.str1.1,"aMS",@progbits,1
.Lanon.718f62d177c65bc51b0cda06dab40e69.0:
	.asciz	"src/lib.rs"
	.size	.Lanon.718f62d177c65bc51b0cda06dab40e69.0, 11

	.type	.Lanon.718f62d177c65bc51b0cda06dab40e69.1,@object
	.section	.data.rel.ro..Lanon.718f62d177c65bc51b0cda06dab40e69.1,"aw",@progbits
	.p2align	3, 0x0
.Lanon.718f62d177c65bc51b0cda06dab40e69.1:
	.quad	.Lanon.718f62d177c65bc51b0cda06dab40e69.0
	.asciz	"\n\000\000\000\000\000\000\000#\000\000\000\t\000\000"
	.size	.Lanon.718f62d177c65bc51b0cda06dab40e69.1, 24

	.type	.Lanon.718f62d177c65bc51b0cda06dab40e69.2,@object
	.section	.data.rel.ro..Lanon.718f62d177c65bc51b0cda06dab40e69.2,"aw",@progbits
	.p2align	3, 0x0
.Lanon.718f62d177c65bc51b0cda06dab40e69.2:
	.quad	.Lanon.718f62d177c65bc51b0cda06dab40e69.0
	.asciz	"\n\000\000\000\000\000\000\000\031\000\000\000\t\000\000"
	.size	.Lanon.718f62d177c65bc51b0cda06dab40e69.2, 24

	.ident	"rustc version 1.101.0-nightly (ea137335b 2026-10-05)"
	.section	".note.GNU-stack","",@progbits

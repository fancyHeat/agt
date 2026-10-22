# Generates the homophone test set for dictionary boosting (docs/benchmarks.md, "Dictionary
# boosting") with Windows' built-in text-to-speech: 16 kHz mono WAVs and cased references.
#
#   powershell -ExecutionPolicy Bypass -File scripts\homophone_set.ps1 [-Out <dir>]
#
# Two sets, each clip once per installed English voice:
# - code-*: dev talk where a dictionary term must come out (main, rebase, kubectl, PR, AGT,
#   Parakeet, and two made-up product names);
# - real-*: sentences where the everyday word must survive (Maine, "the main thing", a pet
#   parakeet, maintain, rebate), including in a code app.
# Run the bench on the folder with the matching dictionary (scripts\homophone_dictionary.toml),
# once with --app Code.exe (code profile) and once with --app notepad.exe (global terms only).
#
# Synthetic speech, not people: clean, evenly paced, one accent per voice. It shows whether
# boosting can flip a homophone and what it breaks, not how often it happens in real use.
# The audio is generated, not stored in the repository; the default output is
# eval-data\homophones (ignored by git).

param([string]$Out = (Join-Path $PSScriptRoot '..\eval-data\homophones'))

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech

# (set, spoken text, reference). The spoken text is what the voice reads (spelled out where
# a voice would mangle the written form); the reference is what should be pasted.
$clips = @(
    @('code', 'push it to main', 'Push it to main.'),
    @('code', 'merge the feature branch into main', 'Merge the feature branch into main.'),
    @('code', 'I rebased onto main this morning', 'I rebased onto main this morning.'),
    @('code', 'git checkout main and pull', 'Git checkout main and pull.'),
    @('code', 'main is broken again', 'Main is broken again.'),
    @('code', 'rebase on top of main before you open the P R', 'Rebase on top of main before you open the PR.'),
    @('code', 'please rebase your branch', 'Please rebase your branch.'),
    @('code', 'do an interactive rebase', 'Do an interactive rebase.'),
    @('code', 'the main branch is protected', 'The main branch is protected.'),
    @('code', 'fast forward main to the release tag', 'Fast forward main to the release tag.'),
    @('code', 'cherry pick that commit onto main', 'Cherry pick that commit onto main.'),
    @('code', 'the P R got merged into main', 'The PR got merged into main.'),
    @('code', 'open a P R for this change', 'Open a PR for this change.'),
    @('code', 'the P R is ready for review', 'The PR is ready for review.'),
    @('code', 'squash it and rebase before merging', 'Squash it and rebase before merging.'),
    @('code', 'run kube cuttle get pods', 'Run kubectl get pods.'),
    @('code', 'use kube cuttle logs to check the crash', 'Use kubectl logs to check the crash.'),
    @('code', 'apply the manifest with kube cuttle', 'Apply the manifest with kubectl.'),
    @('code', 'A G T is running in the tray', 'AGT is running in the tray.'),
    @('code', 'restart A G T after the update', 'Restart AGT after the update.'),
    @('code', 'switch Parakeet to the CPU', 'Switch Parakeet to the CPU.'),
    @('code', 'Parakeet is the speech model we use', 'Parakeet is the speech model we use.'),
    @('code', 'deploy Vexadyne to staging', 'Deploy Vexadyne to staging.'),
    @('code', 'the Vexadyne build failed', 'The Vexadyne build failed.'),
    @('code', 'Quillmate crashed on startup', 'Quillmate crashed on startup.'),
    @('code', 'file a bug against Quillmate', 'File a bug against Quillmate.'),
    @('real', 'we drove up to Maine last summer', 'We drove up to Maine last summer.'),
    @('real', 'the main thing is to stay calm', 'The main thing is to stay calm.'),
    @('real', 'Portland Maine is lovely in the fall', 'Portland, Maine is lovely in the fall.'),
    @('real', 'lobster from Maine is the best', 'Lobster from Maine is the best.'),
    @('real', 'the main course was grilled fish', 'The main course was grilled fish.'),
    @('real', 'her main concern is the budget', 'Her main concern is the budget.'),
    @('real', 'Maine is cold in the winter', 'Maine is cold in the winter.'),
    @('real', 'Maine and Vermont share a border', 'Maine and Vermont share a border.'),
    @('real', 'the main reason I called is the invoice', 'The main reason I called is the invoice.'),
    @('real', 'Bangor Maine is north of Augusta', 'Bangor, Maine is north of Augusta.'),
    @('real', 'a parakeet sat on the windowsill', 'A parakeet sat on the windowsill.'),
    @('real', 'my sister owns a green parakeet', 'My sister owns a green parakeet.'),
    @('real', 'we need to maintain this service', 'We need to maintain this service.'),
    @('real', 'the mainstream media missed it', 'The mainstream media missed it.'),
    @('real', 'we got a rebate on the purchase', 'We got a rebate on the purchase.'),
    @('real', 'the domain name expired yesterday', 'The domain name expired yesterday.'),
    @('real', 'please remain seated', 'Please remain seated.'),
    @('real', 'the main road into town is closed', 'The main road into town is closed.')
)

New-Item -ItemType Directory -Force $Out | Out-Null
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$voices = $synth.GetInstalledVoices() |
    Where-Object { $_.Enabled -and $_.VoiceInfo.Culture.Name -like 'en-*' } |
    ForEach-Object { $_.VoiceInfo.Name }
if (-not $voices) { throw 'no English text-to-speech voice is installed' }
$format = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(
    16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen,
    [System.Speech.AudioFormat.AudioChannel]::Mono)
$utf8 = New-Object System.Text.UTF8Encoding($false)
$n = @{}
foreach ($c in $clips) {
    $set, $spoken, $reference = $c
    $n[$set] = 1 + [int]$n[$set]
    foreach ($voice in $voices) {
        # e.g. "Microsoft David Desktop" -> "david"
        $short = ($voice -split ' ')[1].ToLower()
        $name = '{0}-short-{1:d2}-{2}' -f $set, $n[$set], $short
        $synth.SelectVoice($voice)
        $synth.SetOutputToWaveFile((Join-Path $Out "$name.wav"), $format)
        $synth.Speak($spoken)
        $synth.SetOutputToNull()
        [System.IO.File]::WriteAllText((Join-Path $Out "$name.txt"), "$reference`n", $utf8)
    }
}
$synth.Dispose()
Write-Output ("{0} clips ({1} sentences x {2} voices) in {3}" -f
    ($clips.Count * @($voices).Count), $clips.Count, @($voices).Count, (Resolve-Path $Out))

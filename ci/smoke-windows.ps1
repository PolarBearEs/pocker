param(
    [string]$Bin = "target/debug/pocker.exe"
)

$ErrorActionPreference = "Stop"

function Require-Command {
    param([string]$Name)

    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "missing required tool: $Name"
    }
}

function Invoke-Pocker {
    param([string[]]$PockerArgs)

    & $Bin @PockerArgs
    if ($LASTEXITCODE -ne 0) {
        throw "pocker failed with exit code ${LASTEXITCODE}: $Bin $($PockerArgs -join ' ')"
    }
}

function Invoke-Docker {
    param([string[]]$DockerArgs)

    & docker @DockerArgs
    if ($LASTEXITCODE -ne 0) {
        throw "docker failed with exit code ${LASTEXITCODE}: docker $($DockerArgs -join ' ')"
    }
}

function Test-DockerDaemon {
    & docker version --format "{{.Server.Os}}" *> $null
    return $LASTEXITCODE -eq 0
}

function Wait-DockerDaemon {
    for ($i = 0; $i -lt 60; $i++) {
        if (Test-DockerDaemon) {
            return
        }
        Start-Sleep -Seconds 2
    }

    throw "Docker daemon did not become reachable"
}

function Ensure-DockerDaemon {
    if (Test-DockerDaemon) {
        return
    }

    $service = Get-Service -Name docker -ErrorAction SilentlyContinue
    if ($service) {
        if ($service.Status -ne "Running") {
            Start-Service -Name docker
        }
        Wait-DockerDaemon
        return
    }

    throw "Docker daemon is not reachable and no docker service is installed"
}

function Select-SmokeImage {
    $serverOs = (docker version --format "{{.Server.Os}}").Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "docker daemon is not reachable"
    }
    if ($serverOs -eq "linux") {
        return "registry.k8s.io/pause:3.9"
    }

    $build = [System.Environment]::OSVersion.Version.Build
    if ($build -ge 26000) {
        return "mcr.microsoft.com/windows/nanoserver:ltsc2025"
    }
    if ($build -ge 20348) {
        return "mcr.microsoft.com/windows/nanoserver:ltsc2022"
    }
    if ($build -ge 17763) {
        return "mcr.microsoft.com/windows/nanoserver:ltsc2019"
    }

    throw "unsupported Windows build for Docker smoke image: $build"
}

Require-Command docker
Ensure-DockerDaemon

Write-Host "smoke: Docker daemon is reachable"
Invoke-Docker -DockerArgs @("version", "--format", "{{.Server.Os}}") | Out-Null

Write-Host "smoke: image ls uses the default Windows Docker named pipe"
Remove-Item Env:\DOCKER_HOST -ErrorAction SilentlyContinue
Invoke-Pocker -PockerArgs @("image", "ls")

Write-Host "smoke: image ls accepts explicit npipe Docker host"
$env:DOCKER_HOST = "npipe:////./pipe/docker_engine"
Invoke-Pocker -PockerArgs @("image", "ls")

$image = Select-SmokeImage
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$cacheDir = Join-Path $tempRoot "pocker-windows-smoke-cache"
Remove-Item -Recurse -Force $cacheDir -ErrorAction SilentlyContinue

Write-Host "smoke: pull and load $image through pocker"
docker image rm --force $image *> $null
Invoke-Pocker -PockerArgs @("--cache-dir", $cacheDir, "pull", $image)
Invoke-Docker -DockerArgs @("image", "inspect", $image) | Out-Null

Write-Host "smoke: pull $image into cache without Docker load"
Remove-Item -Recurse -Force $cacheDir -ErrorAction SilentlyContinue
Invoke-Pocker -PockerArgs @("--cache-dir", $cacheDir, "pull", "--no-load", $image)
if (-not (Get-ChildItem -Path (Join-Path $cacheDir "blobs\sha256") -File -ErrorAction SilentlyContinue)) {
    throw "expected cached sha256 blobs after no-load pull"
}

Write-Host "smoke: Windows Docker named-pipe checks passed"

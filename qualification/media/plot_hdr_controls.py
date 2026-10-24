#!/usr/bin/env python3
"""Render the frozen HDR control medians as a standalone comparison figure."""
import argparse
import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('report',type=Path)
args=p.parse_args();v=json.loads(args.report.read_text())
fig,axes=plt.subplots(1,2,figsize=(10,4.5));fig.set_facecolor('#f7f9fc')
for ax,key,title,ylabel in [(axes[0],'median_processing_seconds','Processing 30 seconds of HDR','Elapsed seconds'),(axes[1],'median_cpu_seconds','CPU work for the same output','CPU seconds')]:
    metric=v[key];values=[metric['before'],metric['after']]
    bars=ax.bar(['CPU decode','QSV decode'],values,color=['#66758c','#167e68'],width=.58)
    for bar,value in zip(bars,values):ax.text(bar.get_x()+bar.get_width()/2,value+max(values)*.025,f'{value:.1f}',ha='center',fontsize=12)
    ax.set_title(title,pad=18);ax.set_ylabel(ylabel);ax.set_ylim(0,max(values)*1.3)
    ax.spines[['top','right']].set_visible(False);ax.grid(axis='y',alpha=.18);ax.set_axisbelow(True)
    ax.text(.5,.93,f"{metric['reduction_percent']:.0f}% lower",transform=ax.transAxes,ha='center',color='#167e68',fontweight='bold',fontsize=13)
axes[0].axhline(30,color='#b24a46',linestyle='--',linewidth=1.2)
axes[0].text(.34,31,'30s media',ha='left',color='#9e3c39',fontsize=9,bbox=dict(facecolor='white',edgecolor='none',pad=1))
fig.suptitle('Focused 4K HDR processing comparison',fontsize=17,fontweight='bold',y=.98)
fig.text(.5,.035,'Same 4-CPU limit and FFmpeg 9.0.2 · 2 runs per path · unchanged tone mapper · 48.1 dB output PSNR',ha='center',fontsize=9,color='#445064')
fig.tight_layout(rect=[0,.08,1,.91])
for extension in ['png','svg']:fig.savefig(args.report.with_suffix('.'+extension),dpi=160,facecolor=fig.get_facecolor())
